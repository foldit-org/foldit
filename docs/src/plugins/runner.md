# The Plugin Runner

The molecular backends are not compiled into the client. They run as
out-of-process plugins hosted by **foldit-runner** (the `crates/foldit-runner`
submodule). foldit-runner speaks one protocol to every plugin regardless of the
language it is written in. Rosetta is itself a plugin under
`crates/foldit-runner/plugins/rosetta/`; there is no in-process Rosetta executor.

## Orchestrator

`foldit_runner::Orchestrator` is the host-facing type. The core's `RunnerClient`
holds one (`crates/foldit-core/src/runner_client/mod.rs`) and reaches into it to
discover plugins, dispatch ops and queries, and pump per-frame updates. The
orchestrator owns the worker process pool and the entity-lock table; dispatch
flows through the unified plugin path, so adding a backend does not add a code
path in core.

A new orchestrator is installed on each structure load
(`init_orchestrator`), and `reset_for_new_structure` releases locks and drops
the per-plugin sessions (the warm workers stay up) so each plugin re-`Init`s
against the new structure.

## Bring-up: kick and poll

Plugin bring-up is non-blocking; it never stalls the loading screen. Each phase
is a kick that starts work and a poll that finishes whatever completed since the
last frame. The startup state machine in
`crates/foldit-core/src/app/startup.rs` accumulates completions across frames
against an expected set while the host keeps rendering.

- **Warm** (`kick_warms` / `poll_warms`). Discover plugins under the plugins
  root, then for each one bind a listener and spawn the worker child. The
  connects finish on later polls. A plugin whose kick fails is dropped from the
  expected set so bring-up cannot hang on it; failures degrade to "that plugin
  disabled", never an abort.
- **Init** (`kick_inits` / `poll_inits`). For each warmed plugin, send an `Init`
  against the loaded assembly, carrying the puzzle payload (ligand assets,
  catalytic constraints, and config params such as the weight patch and
  objective filters). Each plugin replies with its normalized post-`Init`
  assembly, which the host adopts.
- **Score**. Once a session is up, the first score is requested and stamped, and
  the App flips to `InSession`.

Ops dispatched during play follow the same model: streaming op frames arrive as
tentative edits, and the orchestrator's pump is drained each tick
(`apply_backend_updates`). Queries (used by the structural overlays) can be fired
non-blocking (`request_query` / `poll_query_results`) or resolved synchronously
(`dispatch_plugin_query`) for panel-initiated reads.

## Workers and IPC

The worker binary is **`foldit-worker`** (`crates/foldit-runner`, built by
`cargo xtask build-host`). Each plugin runs in its own worker process. The
host-to-worker channel is native-only: control messages travel over an
interprocess socket, and bulk data (assemblies, score payloads) travels over
iceoryx2 shared memory (`crates/foldit-runner/src/ipc/`). On `SIGINT`/`SIGTERM`
the desktop binary kills the worker process groups so Python subprocesses are not
orphaned.

See [Python and Native Plugins](hosts.md) for how a worker loads each plugin and
[Plugin SDK and Protocol](sdk.md) for the protocol and manifest.
