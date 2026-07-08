//! Worker process runner — plugin protocol dispatcher.
//!
//! Loads ONE plugin per worker process (selected at spawn time via the
//! manifest at `<plugin_dir>/plugin.toml`), then services
//! [`proto::PluginRequest`] envelopes from the orchestrator. The op-id /
//! query-id is opaque to the runner; it forwards the string verbatim to
//! the corresponding [`Plugin`] trait method via the manifest's chosen
//! impl (`PyPlugin` from the `foldit-python-host` cdylib for Python
//! plugins, [`crate::plugin::native::NativePlugin`] for native dylibs).
//!
//! Pull-based streaming: the worker never initiates traffic. Streaming
//! intermediates ride inside `PollStream` polls.
//!
//! CLI shape — exactly two positional args:
//!
//! ```text
//! foldit-worker <plugin_dir> <ipc_endpoint>
//! ```
//!
//! The worker reads `<plugin_dir>/plugin.toml` to learn the plugin kind
//! and entry point. For Python plugins the orchestrator sets `PYTHONHOME`
//! and puts the env's `lib/` on the loader path before spawning, so the
//! embedded interpreter boots from the per-plugin conda env. The worker
//! never invokes pixi; pixi only creates those envs at dev/build time.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use foldit_plugin_sdk::decode::{
    dispatch_context_from_proto, params_from_proto,
};
use foldit_plugin_sdk::PluginError;
use prost::Message;

use crate::ipc::{self, receive_message, send_message};
use crate::orchestrator::manifest::PluginManifest;
use crate::orchestrator::PollOutcome;
use crate::plugin::{load_plugin_from_manifest, AssemblyPayload, Plugin};
use crate::proto::plugin as proto;

/// Top-level driver for the `foldit-worker` binary. Owns the loaded
/// plugin and the orchestrator-facing socket.
pub struct WorkerRunner {
    stream: ipc::LocalSocketStream,
    plugin: Box<dyn Plugin>,
}

impl WorkerRunner {
    /// Parse argv, load the plugin from its manifest, connect the
    /// orchestrator socket, and run the request loop until the
    /// orchestrator closes the connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the manifest can't be read, the plugin can't
    /// be loaded, the socket connection fails, or the request loop
    /// hits a fatal I/O error.
    pub fn run() -> Result<()> {
        env_logger::init();
        log::info!("Spawned plugin worker process ({})", std::process::id());

        let args: Vec<String> = env::args().collect();
        if args.len() != 3 {
            log::error!("Usage: foldit-worker <plugin_dir> <ipc_endpoint>");
            std::process::exit(1);
        }
        let plugin_dir = PathBuf::from(&args[1]);
        let socket_name = args[2].clone();

        let manifest_path = plugin_dir.join("plugin.toml");
        let manifest_src = std::fs::read_to_string(&manifest_path)
            .with_context(|| {
                format!("read manifest {}", manifest_path.display())
            })?;
        let manifest = PluginManifest::parse(&manifest_src).map_err(|e| {
            anyhow::anyhow!("parse manifest {}: {}", manifest_path.display(), e)
        })?;
        log::info!(
            "loaded manifest for plugin id={} kind={:?}",
            manifest.id,
            manifest.kind
        );

        // Python plugins: load_plugin_from_manifest dlopens
        // libfoldit_python_host (which has libpython as DT_NEEDED) and
        // delegates the Python interpreter setup + plugin module load
        // to it. Native plugins: dlopen the plugin dylib directly.
        // Worker binary itself never link-depends on libpython.
        let plugin = load_plugin_from_manifest(&plugin_dir, &manifest)
            .with_context(|| {
                format!("load plugin from {}", plugin_dir.display())
            })?;
        log::info!("plugin {} loaded", manifest.id);

        let stream =
            ipc::connect_to_socket(&socket_name).with_context(|| {
                format!("failed to connect to socket {socket_name}")
            })?;
        log::info!("Worker connected via socket {socket_name}");

        let mut runner = WorkerRunner { stream, plugin };
        runner.request_loop()
    }

    fn request_loop(&mut self) -> Result<()> {
        loop {
            let request_bytes = match receive_message(&mut self.stream) {
                Ok(b) => b,
                Err(e) => {
                    let s = e.to_string();
                    if s.contains("UnexpectedEof")
                        || s.contains("failed to read message length")
                    {
                        log::info!("Worker shutting down (connection closed)");
                        return Ok(());
                    }
                    return Err(e).context("failed to receive PluginRequest");
                }
            };

            let request = proto::PluginRequest::decode(&request_bytes[..])
                .context("failed to decode PluginRequest")?;
            let response = self.handle_request(request);
            let response_bytes = response.encode_to_vec();
            send_message(&mut self.stream, &response_bytes)
                .context("failed to send PluginResponse")?;
        }
    }

    fn handle_request(
        &self,
        req: proto::PluginRequest,
    ) -> proto::PluginResponse {
        use proto::plugin_request::Request as Req;
        use proto::plugin_response::Response as Resp;

        let resp = match req.request {
            Some(Req::Init(r)) => Resp::Init(self.handle_init(&r)),
            Some(Req::UpdateAssembly(r)) => {
                Resp::UpdateAssembly(self.handle_update_assembly(&r))
            }
            Some(Req::Drop(r)) => Resp::Drop(self.handle_drop(r)),
            Some(Req::Invoke(r)) => Resp::Invoke(self.handle_invoke(r)),
            Some(Req::StartStream(r)) => {
                Resp::StartStream(self.handle_start_stream(r))
            }
            Some(Req::PollStream(r)) => {
                Resp::PollStream(self.handle_poll_stream(r))
            }
            Some(Req::UpdateStream(r)) => {
                Resp::UpdateStream(self.handle_update_stream(r))
            }
            Some(Req::CancelStream(r)) => {
                Resp::CancelStream(self.handle_cancel_stream(r))
            }
            Some(Req::Query(r)) => Resp::Query(self.handle_query(r)),
            None => {
                log::warn!("received empty PluginRequest envelope");
                Resp::Invoke(proto::InvokeResponse {
                    response: Some(proto::invoke_response::Response::Error(
                        proto::Error {
                            code: "INVALID_REQUEST".into(),
                            message: "PluginRequest had no oneof variant"
                                .into(),
                            details: HashMap::new(),
                        },
                    )),
                })
            }
        };
        proto::PluginResponse {
            response: Some(resp),
        }
    }

    // Lifecycle handlers

    fn handle_init(&self, req: &proto::InitRequest) -> proto::InitResponse {
        // The generic `params` channel (weight-patch + objective filters) is
        // forwarded to the plugin's init; the bridge consumes it for pose
        // build + puzzle config. `assets` (ligand bytes) / `constraints`
        // (catalytic) are still log-only here — their bridge-side consumption
        // is a separate step.
        if !req.assets.is_empty() || !req.constraints.is_empty() {
            let asset_names: Vec<&str> =
                req.assets.iter().map(|a| a.name.as_str()).collect();
            log::info!(
                "init: received {} ligand asset(s), {} constraint(s); \
                 assets={:?}",
                req.assets.len(),
                req.constraints.len(),
                asset_names,
            );
        }

        let params = params_from_proto(req.params.clone());
        let (session, initial_assembly) =
            match self.plugin.init(&req.assembly, &req.assets, &params) {
                Ok(s) => s,
                Err(e) => {
                    return proto::InitResponse {
                        response: Some(proto::init_response::Response::Error(
                            error_from(&e),
                        )),
                    }
                }
            };
        match self.plugin.register() {
            Ok(reg) => proto::InitResponse {
                response: Some(proto::init_response::Response::Success(
                    proto::InitSuccess {
                        session,
                        registration: Some(reg),
                        initial_assembly,
                    },
                )),
            },
            Err(e) => proto::InitResponse {
                response: Some(proto::init_response::Response::Error(
                    error_from(&e),
                )),
            },
        }
    }

    fn handle_update_assembly(
        &self,
        req: &proto::UpdateAssemblyRequest,
    ) -> proto::UpdateAssemblyResponse {
        let payload = match &req.payload {
            Some(proto::update_assembly_request::Payload::Full(b)) => {
                AssemblyPayload::Full(b)
            }
            Some(proto::update_assembly_request::Payload::Delta(b)) => {
                AssemblyPayload::Delta(b)
            }
            None => {
                return proto::UpdateAssemblyResponse {
                    error: Some(proto::Error {
                        code: "INVALID_REQUEST".into(),
                        message: "UpdateAssemblyRequest payload oneof unset"
                            .into(),
                        details: HashMap::new(),
                    }),
                };
            }
        };
        match self.plugin.update_assembly(
            req.session,
            payload,
            req.from_gen,
            req.to_gen,
        ) {
            Ok(()) => proto::UpdateAssemblyResponse { error: None },
            Err(e) => proto::UpdateAssemblyResponse {
                error: Some(error_from(&e)),
            },
        }
    }

    fn handle_drop(&self, req: proto::DropRequest) -> proto::DropResponse {
        match self.plugin.drop_session(req.session) {
            Ok(()) => proto::DropResponse { error: None },
            Err(e) => proto::DropResponse {
                error: Some(error_from(&e)),
            },
        }
    }

    // Op handlers

    fn handle_invoke(
        &self,
        req: proto::InvokeRequest,
    ) -> proto::InvokeResponse {
        let ctx = dispatch_context_from_proto(req.context);
        let params = params_from_proto(req.params);
        match self.plugin.invoke(req.session, &req.op, &ctx, &params) {
            Ok(bytes) => proto::InvokeResponse {
                response: Some(proto::invoke_response::Response::Assembly(
                    bytes,
                )),
            },
            Err(e) => proto::InvokeResponse {
                response: Some(proto::invoke_response::Response::Error(
                    error_from(&e),
                )),
            },
        }
    }

    fn handle_start_stream(
        &self,
        req: proto::StartStreamRequest,
    ) -> proto::StartStreamResponse {
        let ctx = dispatch_context_from_proto(req.context);
        let params = params_from_proto(req.params);
        match self.plugin.start_stream(
            req.session,
            &req.op,
            &ctx,
            &params,
            req.request_id,
        ) {
            Ok(()) => proto::StartStreamResponse { error: None },
            Err(e) => proto::StartStreamResponse {
                error: Some(error_from(&e)),
            },
        }
    }

    // One arm per `PollOutcome` variant re-encoded back into the proto
    // oneof; the body is per-variant serialization, not splittable
    // duplication.
    #[allow(clippy::too_many_lines)]
    fn handle_poll_stream(
        &self,
        req: proto::PollStreamRequest,
    ) -> proto::PollStreamResponse {
        match self.plugin.poll_stream(req.request_id) {
            Ok(PollOutcome::Pending {
                latest_assembly,
                progress,
                stage,
                score,
            }) => proto::PollStreamResponse {
                result: Some(proto::poll_stream_response::Result::Pending(
                    proto::StreamPending {
                        latest_assembly: latest_assembly.unwrap_or_default(),
                        progress,
                        stage,
                        score,
                    },
                )),
            },
            Ok(PollOutcome::Checkpoint {
                latest_assembly,
                progress,
                stage,
                score,
            }) => proto::PollStreamResponse {
                result: Some(proto::poll_stream_response::Result::Checkpoint(
                    proto::StreamCheckpoint {
                        latest_assembly: latest_assembly.unwrap_or_default(),
                        progress,
                        stage,
                        score,
                    },
                )),
            },
            Ok(PollOutcome::Cancelled { assembly, score }) => {
                proto::PollStreamResponse {
                    result: Some(
                        proto::poll_stream_response::Result::Cancelled(
                            proto::StreamCancelled { assembly, score },
                        ),
                    ),
                }
            }
            Ok(PollOutcome::Final { assembly, score }) => {
                proto::PollStreamResponse {
                    result: Some(proto::poll_stream_response::Result::Final(
                        proto::StreamFinal { assembly, score },
                    )),
                }
            }
            Ok(PollOutcome::Error {
                code,
                message,
                details,
            }) => proto::PollStreamResponse {
                result: Some(proto::poll_stream_response::Result::Error(
                    proto::Error {
                        code,
                        message,
                        details,
                    },
                )),
            },
            Err(e) => proto::PollStreamResponse {
                result: Some(proto::poll_stream_response::Result::Error(
                    error_from(&e),
                )),
            },
        }
    }

    fn handle_update_stream(
        &self,
        req: proto::UpdateStreamRequest,
    ) -> proto::UpdateStreamResponse {
        let params = params_from_proto(req.params);
        match self.plugin.update_stream(req.request_id, &params) {
            Ok(()) => proto::UpdateStreamResponse { error: None },
            Err(e) => proto::UpdateStreamResponse {
                error: Some(error_from(&e)),
            },
        }
    }

    fn handle_cancel_stream(
        &self,
        req: proto::CancelStreamRequest,
    ) -> proto::CancelStreamResponse {
        match self.plugin.cancel_stream(req.request_id) {
            Ok(()) => proto::CancelStreamResponse { error: None },
            Err(e) => proto::CancelStreamResponse {
                error: Some(error_from(&e)),
            },
        }
    }

    // Query handler

    fn handle_query(&self, req: proto::QueryRequest) -> proto::QueryResponse {
        let ctx = dispatch_context_from_proto(req.context);
        let params = params_from_proto(req.params);
        let assembly = req.assembly.as_deref().unwrap_or(&[]);
        match self.plugin.query(
            req.session,
            &req.query,
            &ctx,
            &params,
            assembly,
        ) {
            Ok(bytes) => proto::QueryResponse {
                response: Some(proto::query_response::Response::Data(bytes)),
            },
            Err(e) => proto::QueryResponse {
                response: Some(proto::query_response::Response::Error(
                    error_from(&e),
                )),
            },
        }
    }
}

/// Entry point used by the `foldit-worker` binary.
///
/// # Errors
///
/// Propagates the error returned by [`WorkerRunner::run`].
pub fn main() -> Result<()> {
    WorkerRunner::run()
}

fn error_from(e: &PluginError) -> proto::Error {
    proto::Error {
        code: "INTERNAL".into(),
        message: e.to_string(),
        details: HashMap::new(),
    }
}
