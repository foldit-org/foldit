//! Packing-void query path.
//!
//! Forward the well-known `voids` query to the orchestrator over the
//! generic raw-bytes dispatch (the reply is opaque proto `VoidField`
//! bytes that `App` decodes into a distance field the viso engine meshes;
//! voids go to the viso engine, not an orchestrator score merge, so this
//! stays on the generic query path rather than the score-specialized one).
//!
//! Until a plugin advertises the `voids` query, this is an inert no-op:
//! [`Self::supports_voids`] reports `false` and the caller never requests.

#[cfg(not(target_arch = "wasm32"))]
use super::RunnerClient;

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Whether any plugin has registered the `voids` query. The bridge
    /// advertises a query by registration (same index the `score` query
    /// lives in), so this is the host-side support gate: the at-rest
    /// trigger requests voids ONLY when this is `true`, keeping the path
    /// inert until a plugin implements `voids`. `false` when no
    /// orchestrator is installed.
    pub(crate) fn supports_voids(&self) -> bool {
        self.orchestrator
            .as_ref()
            .is_some_and(|orch| orch.plugin_registry().get_query("voids").is_some())
    }

    /// Request the `voids` query synchronously and return its raw opaque
    /// bytes (proto `VoidField`), the payload `App` decodes into a field.
    /// Passes no bytes and the default dispatch context: the query covers
    /// the current session pose, like the whole-assembly `score` query.
    ///
    /// Returns an empty `Vec` (the "no voids" / "clear" signal) when no
    /// orchestrator is installed, no plugin advertises the query, or the
    /// query errors. The unsupported case is filtered by
    /// [`Self::supports_voids`] before the call; the error case is
    /// swallowed at `trace` level so an at-rest miss never spams the log.
    pub(crate) fn request_voids_bytes(&mut self) -> Vec<u8> {
        use foldit_runner::orchestrator::DispatchContext;
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        match orch.dispatch_query("voids", DispatchContext::default(), std::collections::HashMap::new()) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::trace!("[RunnerClient] voids query failed: {e}");
                Vec::new()
            }
        }
    }
}
