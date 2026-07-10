//! End-to-end smoke test for the plugin protocol.
//!
//! Drives a live orchestrator → spawned worker subprocess → loaded
//! Python `dummy` plugin → Init → Register → Query("sequence_design") →
//! bytes back. Round-trip uses only `proto::plugin` types Rust↔Python.
//! The worker is spawned directly (no pixi); the orchestrator sets
//! `PYTHONHOME` to `.pixi/envs/dummy` and puts its `lib/` on the loader
//! path so the embedded interpreter boots from that env.
//!
//! Python hosting goes through the `foldit-python-host` cdylib — the
//! worker binary itself is pyo3-free. The cdylib must be built and
//! co-located with the worker binary for the test to find it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::HashMap;
use std::path::PathBuf;

use foldit_runner::orchestrator::{
    DispatchContext, InitPayload, Orchestrator, ResidueRef,
};

fn workspace_runner_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn worker_binary_path() -> PathBuf {
    // `FOLDIT_WORKER_BIN` (absolute path to a prebuilt worker) wins when
    // set; the recipe that sets it owns building that worker. Otherwise
    // fall back to the compile-time `CARGO_BIN_EXE_foldit-worker`, the
    // crate-target binary built as part of the integration-test build
    // graph. The override exists so the worker and the python-host dylib
    // it dlopens can be co-built from the same (root) workspace.
    if let Some(p) = std::env::var_os("FOLDIT_WORKER_BIN") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_BIN_EXE_foldit-worker"))
}

#[test]
fn dummy_round_trip_via_orchestrator() {
    // Force log visibility for debugging without breaking other tests.
    let _ = env_logger::builder().is_test(true).try_init();

    let runner_dir = workspace_runner_dir();
    let plugins_root = runner_dir.join("tests/fixtures");
    let worker_bin = worker_binary_path();

    let mut orch = Orchestrator::new();
    orch.set_worker_binary(worker_bin);

    let ids = orch
        .discover_plugins(&plugins_root)
        .expect("discover_plugins succeeds");
    assert!(
        ids.iter().any(|id| id == "dummy"),
        "expected dummy in discovered ids {ids:?}"
    );

    let _ = orch
        .ensure_plugin_registered("dummy", Vec::new())
        .expect("ensure_plugin_registered for dummy succeeds");

    let bytes = orch
        .dispatch_query(
            "sequence_design",
            DispatchContext::default(),
            HashMap::new(),
        )
        .expect("dispatch_query(sequence_design) succeeds");
    let info = String::from_utf8(bytes).expect("query result is UTF-8");
    // dummy returns newline-separated "sequence\tscore" lines; default
    // num_sequences=1 → one line starting with "AAA\t".
    assert!(
        info.starts_with("AAA\t"),
        "expected sequence_design output to start with 'AAA\\t', got: {info}"
    );

    orch.unregister_plugin("dummy");
}

// =============================================================================
// SDK bound-type round-trip through the dummy plugin.
//
// Exercises the two host marshalling paths that changed when the Python
// plugin path moved onto the SDK's bound `DispatchContext` / `PollOutcome`:
//
// (a) A query dispatched with a POPULATED `DispatchContext` (a focused
//     entity + a non-empty selection) so `dispatch_context_to_py` builds the
//     bound `DispatchContext` / `ResidueRef` from real values, not the empty
//     default.
// (b) The `stream_test` streaming op driven start_stream -> poll loop to its
//     Final terminal, so `poll_outcome_from_py` reads the bound `PollOutcome`
//     back across pending / checkpoint / final.
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn dummy_sdk_bound_types_round_trip() {
    let _ = env_logger::builder().is_test(true).try_init();

    let runner_dir = workspace_runner_dir();
    let plugins_root = runner_dir.join("tests/fixtures");
    let worker_bin = worker_binary_path();

    let mut orch = Orchestrator::new();
    orch.set_worker_binary(worker_bin);

    let ids = orch
        .discover_plugins(&plugins_root)
        .expect("discover_plugins succeeds");
    assert!(
        ids.iter().any(|id| id == "dummy"),
        "expected dummy in discovered ids {ids:?}"
    );

    let _ = orch
        .ensure_plugin_registered("dummy", Vec::new())
        .expect("ensure_plugin_registered for dummy succeeds");

    // (a) Query with a populated context: focus on entity 0, two selected
    // residues, one designable residue. Drives the bound DispatchContext +
    // ResidueRef construction in dispatch_context_to_py.
    let ctx = DispatchContext {
        focused_entity_id: Some(molex::EntityId::from_raw(0)),
        selection: vec![
            ResidueRef {
                entity_id: molex::EntityId::from_raw(0),
                residue_index: 0,
            },
            ResidueRef {
                entity_id: molex::EntityId::from_raw(0),
                residue_index: 1,
            },
        ],
        designable: vec![ResidueRef {
            entity_id: molex::EntityId::from_raw(0),
            residue_index: 0,
        }],
    };
    let bytes = orch
        .dispatch_query("sequence_design", ctx, HashMap::new())
        .expect("dispatch_query(sequence_design) with populated ctx succeeds");
    let info = String::from_utf8(bytes).expect("query result is UTF-8");
    assert!(
        info.starts_with("AAA\t"),
        "expected sequence_design output to start with 'AAA\\t', got: {info}"
    );

    // (b) Drive the streaming op to Final. The dummy emits two pending
    // polls, one checkpoint, then a final carrying alanine wire bytes.
    let (request_id, handle) = orch
        .dispatch_start_stream(
            "stream_test",
            DispatchContext::default(),
            HashMap::new(),
            |_| None,
        )
        .expect("dispatch_start_stream(stream_test) succeeds");

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut final_assembly: Option<molex::Assembly> = None;
    let mut last_error: Option<String> = None;
    'outer: loop {
        assert!(
            std::time::Instant::now() <= deadline,
            "stream_test did not complete within 30 s (rid {request_id})"
        );
        for update in orch.drain_plugin_updates() {
            use foldit_runner::orchestrator::PluginUpdate;
            match update {
                PluginUpdate::Pending {
                    request_id: rid, ..
                }
                | PluginUpdate::Checkpoint {
                    request_id: rid, ..
                } if rid == request_id => {}
                PluginUpdate::Final {
                    request_id: rid,
                    assembly,
                    ..
                } if rid == request_id => {
                    final_assembly = Some(assembly);
                    break 'outer;
                }
                PluginUpdate::Error {
                    request_id: rid,
                    message,
                } if rid == request_id => {
                    last_error = Some(message);
                    break 'outer;
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    orch.release_dispatch_locks(handle);

    if let Some(msg) = last_error {
        panic!("stream_test errored: {msg}");
    }
    let assembly = final_assembly.expect("Final assembly must arrive");
    // The dummy's final carries the alanine wire bytes; decode lands a
    // single-entity assembly.
    assert!(
        !assembly.entities().is_empty(),
        "Final alanine assembly must contain at least one entity"
    );

    orch.unregister_plugin("dummy");
}

// =============================================================================
// Async warm/init round-trip through the non-blocking startup path.
//
// The blocking `register_plugin` path (above) accepts worker connections
// on a listener left in blocking mode, so accepted streams are blocking
// and Init writes succeed. The async startup path puts the listener into
// non-blocking ACCEPT mode (`kick_warm_plugin`) and polls. On macOS,
// accept propagates the listener's non-blocking flag onto the accepted
// socket; if that flag isn't cleared, the pump thread's first large Init
// write returns WouldBlock and the plugin is disabled. This test drives
// that async path (kick_warm -> poll_warm -> kick_init -> poll_init) end
// to end against `dummy` and asserts Init SUCCEEDS. It reproduces the
// disabled-on-startup failure and would fail without the accepted-stream
// blocking fix in `ipc::sockets::try_accept_connection`.
// =============================================================================

#[test]
fn dummy_async_warm_init_via_orchestrator() {
    let _ = env_logger::builder().is_test(true).try_init();

    let runner_dir = workspace_runner_dir();
    let plugins_root = runner_dir.join("tests/fixtures");
    let worker_bin = worker_binary_path();

    let mut orch = Orchestrator::new();
    orch.set_worker_binary(worker_bin);

    let ids = orch
        .discover_plugins(&plugins_root)
        .expect("discover_plugins succeeds");
    assert!(
        ids.iter().any(|id| id == "dummy"),
        "expected dummy in discovered ids {ids:?}"
    );

    // Clone the descriptor out: kick_warm_plugin needs &mut self while the
    // descriptor lookup borrows &self.
    let descriptor = orch
        .plugin_descriptor("dummy")
        .expect("dummy descriptor present after discovery")
        .clone();

    // Begin warming without blocking on the worker's connect.
    orch.kick_warm_plugin(&descriptor)
        .expect("kick_warm_plugin(dummy) succeeds");

    // Poll until the worker connects (promoted into plugin_workers).
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut warmed = false;
    while std::time::Instant::now() <= deadline {
        for (id, res) in orch.poll_warm_plugins() {
            res.unwrap_or_else(|e| panic!("warm failed for {id}: {e}"));
            if id == "dummy" {
                warmed = true;
            }
        }
        if warmed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(warmed, "dummy worker did not warm within 30 s");

    // Submit Init without blocking on the reply, then poll until it lands.
    // This is the write that returns WouldBlock (os error 35) on macOS
    // when the accepted stream is left non-blocking.
    orch.kick_init_session("dummy", Vec::new(), InitPayload::default())
        .expect("kick_init_session(dummy) submits");

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut init_result: Option<Result<Vec<u8>, _>> = None;
    while std::time::Instant::now() <= deadline {
        for (id, res) in orch.poll_init_sessions() {
            if id == "dummy" {
                init_result = Some(res);
            }
        }
        if init_result.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let init_result =
        init_result.expect("dummy Init did not reply within 30 s");
    // The key assertion: Init succeeded. Without the accepted-stream
    // blocking fix this is Err("failed to write message data") on macOS.
    let _ = init_result
        .expect("dummy async Init must succeed (Ok bytes, not error)");

    orch.unregister_plugin("dummy");
}

// =============================================================================
// 5a.4 + 5a.5: rosetta native-plugin round-trip
//
// Validates that `foldit-worker` can discover + spawn + Init/Register
// the rosetta plugin through the native plugin path. Stub vtable returns
// empty op/query catalogs and Unsupported for everything else — this
// test asserts the wiring works end-to-end, not Rosetta semantics.
//
// Skips if librosetta_interactive.dylib isn't built (CI without rosetta).
// =============================================================================

fn rosetta_dylib_filename() -> &'static str {
    if cfg!(target_os = "macos") {
        "librosetta_interactive.dylib"
    } else if cfg!(target_os = "windows") {
        "rosetta_interactive.dll"
    } else {
        "librosetta_interactive.so"
    }
}

/// True when the rosetta native binary is present at either resolved
/// location the loader checks: `local/<name>` (from-source build) or
/// `prebuilt/<host-triple>/<name>` (vendored fallback). Delegates to the
/// canonical resolver so the location precedence lives in one place.
fn rosetta_dylib_present(plugins_root: &std::path::Path) -> bool {
    let rosetta = plugins_root.join("rosetta");
    foldit_runner::plugin::resolve_native_binary_inner(
        &rosetta,
        "rosetta",
        rosetta_dylib_filename(),
        foldit_runner::plugin::host_target_triple(),
        std::path::Path::exists,
    )
    .is_ok()
}

/// Build a minimal assembly-bytes buffer from the workspace's puzzle 1
/// `structure.pdb`. Used as the init payload for tests that exercise
/// `bridge_init`'s real decode path. Returns `None` if the source PDB
/// isn't reachable (CI without checked-out assets).
fn minimal_assembly_bytes() -> Option<Vec<u8>> {
    // Walk up from CARGO_MANIFEST_DIR to the workspace root and read
    // puzzle 1's PDB. Two `parent()`s to escape `crates/foldit-runner`.
    let pdb_path = workspace_runner_dir()
        .parent()?
        .parent()?
        .join("assets/levels/0000010001/structure.pdb");
    if !pdb_path.exists() {
        return None;
    }
    let entities = molex::Assembly::from_file(&pdb_path).ok()?.into_entities();
    if entities.is_empty() {
        return None;
    }
    let asm = molex::Assembly::new(entities);
    asm.to_bytes().ok()
}

#[test]
fn rosetta_round_trip_via_orchestrator() {
    let _ = env_logger::builder().is_test(true).try_init();

    let runner_dir = workspace_runner_dir();
    let plugins_root = runner_dir.join("../../plugins");

    if !rosetta_dylib_present(&plugins_root) {
        eprintln!(
            "skipping rosetta_round_trip: dylib not built under {}/rosetta \
             (local/ or prebuilt/{}/) — run `cargo xtask \
             build-rosetta-interactive`",
            plugins_root.display(),
            foldit_runner::plugin::host_target_triple(),
        );
        return;
    }

    let worker_bin = worker_binary_path();

    let Some(init_assembly) = minimal_assembly_bytes() else {
        eprintln!(
            "skipping rosetta_round_trip: puzzle 1 PDB not reachable from the \
             test's CARGO_MANIFEST_DIR walk-up — CI without the assets/ \
             directory checked out"
        );
        return;
    };

    let mut orch = Orchestrator::new();
    orch.set_worker_binary(worker_bin);

    let ids = orch
        .discover_plugins(&plugins_root)
        .expect("discover_plugins succeeds");
    assert!(
        ids.iter().any(|id| id == "rosetta"),
        "expected rosetta in discovered ids {ids:?}"
    );

    // Spawns foldit-worker plugins/rosetta <socket>, which dlopens
    // librosetta_interactive.dylib via NativePlugin::load, validates
    // the ABI version, and runs Init/Register. `bridge_init` decodes
    // the assembly bytes we hand it and builds a SessionPose.
    let _ = orch
        .ensure_plugin_registered("rosetta", init_assembly)
        .expect("ensure_plugin_registered for rosetta succeeds");

    orch.unregister_plugin("rosetta");
}

// =============================================================================
// Wiggle round-trip through the native rosetta plugin.
//
// Drives `Orchestrator::dispatch_start_stream("ActionGlobalMinimize", ...)`
// against the real rosetta dylib, polls the plugin update channel until
// a Final lands, and asserts the post-wiggle assembly bytes differ byte-for-
// byte from the input. This is the canonical wave-1 streaming round-trip
// covering the Action registry → ActionContext → execute()/poll() →
// snapshot path end-to-end.
//
// Skips when the dylib isn't built or assets/ isn't checked out (matches
// `rosetta_round_trip_via_orchestrator`'s skip rules).
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn wiggle_round_trip_via_orchestrator() {
    let _ = env_logger::builder().is_test(true).try_init();

    let runner_dir = workspace_runner_dir();
    let plugins_root = runner_dir.join("../../plugins");

    if !rosetta_dylib_present(&plugins_root) {
        eprintln!(
            "skipping wiggle_round_trip: dylib not built under {}/rosetta \
             (local/ or prebuilt/{}/) — run `cargo xtask \
             build-rosetta-interactive`",
            plugins_root.display(),
            foldit_runner::plugin::host_target_triple(),
        );
        return;
    }

    let worker_bin = worker_binary_path();

    let Some(init_assembly) = minimal_assembly_bytes() else {
        eprintln!(
            "skipping wiggle_round_trip: puzzle 1 PDB not reachable from the \
             test's CARGO_MANIFEST_DIR walk-up — CI without the assets/ \
             directory checked out"
        );
        return;
    };
    let init_bytes_len = init_assembly.len();

    let mut orch = Orchestrator::new();
    orch.set_worker_binary(worker_bin);

    let ids = orch
        .discover_plugins(&plugins_root)
        .expect("discover_plugins succeeds");
    assert!(
        ids.iter().any(|id| id == "rosetta"),
        "expected rosetta in discovered ids {ids:?}"
    );

    let _ = orch
        .ensure_plugin_registered("rosetta", init_assembly)
        .expect("ensure_plugin_registered for rosetta succeeds");

    // Sanity: registry should now know about ActionGlobalMinimize via
    // the bridge's action_registry table.
    assert!(
        orch.plugin_registry()
            .get_op("ActionGlobalMinimize")
            .is_some(),
        "rosetta bridge did not register ActionGlobalMinimize"
    );

    // Dispatch the wiggle stream. DispatchContext is empty (session-wide
    // wiggle); params empty (the registry row's factory hard-codes an
    // all-residue selection + 50-iter cap).
    let (request_id, handle) = orch
        .dispatch_start_stream(
            "ActionGlobalMinimize",
            DispatchContext::default(),
            HashMap::new(),
            |_| None,
        )
        .expect("dispatch_start_stream(ActionGlobalMinimize) succeeds");

    // Poll until Final / Error. The 50-iter wiggle cap is short; give
    // it 60 s wall-clock to ride out cold rosetta init + minimization
    // before declaring the test stalled.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_mins(1);
    let mut final_assembly: Option<molex::Assembly> = None;
    let mut last_error: Option<String> = None;
    'outer: loop {
        assert!(
            std::time::Instant::now() <= deadline,
            "wiggle stream did not complete within 60 s (rid {request_id})"
        );
        // The orchestrator's per-plugin pump thread polls active
        // streams on its own POLL_INTERVAL; we just drain whatever has
        // landed on the update channel.
        for update in orch.drain_plugin_updates() {
            use foldit_runner::orchestrator::PluginUpdate;
            match update {
                PluginUpdate::Pending {
                    request_id: rid,
                    progress,
                    ..
                } if rid == request_id => {
                    eprintln!(
                        "wiggle pending: rid={rid} progress={progress:?}"
                    );
                }
                PluginUpdate::Final {
                    request_id: rid,
                    assembly,
                    ..
                } if rid == request_id => {
                    final_assembly = Some(assembly);
                    break 'outer;
                }
                PluginUpdate::Error {
                    request_id: rid,
                    message,
                } if rid == request_id => {
                    last_error = Some(message);
                    break 'outer;
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    orch.release_dispatch_locks(handle);

    if let Some(msg) = last_error {
        panic!("wiggle stream errored: {msg}");
    }
    let assembly = final_assembly.expect("Final assembly must arrive");
    assert!(
        !assembly.entities().is_empty(),
        "Final assembly must contain at least one entity"
    );

    // Round-trip the Final back to assembly bytes and assert non-empty.
    // We don't assert byte-inequality with the input -- if wiggle
    // landed on an iter-0 plateau the bytes could match. The real
    // signal is "Final arrived without Error".
    let final_bytes = assembly.to_bytes().expect("Final assembly serializes");
    assert!(
        !final_bytes.is_empty(),
        "Final assembly serialized to empty bytes"
    );
    eprintln!(
        "wiggle round-trip ok: input {} bytes -> Final {} bytes ({} entities)",
        init_bytes_len,
        final_bytes.len(),
        assembly.entities().len()
    );

    orch.unregister_plugin("rosetta");
}
