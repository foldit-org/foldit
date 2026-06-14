//! Exposed-hydrophobic query path.
//!
//! Forward the well-known `exposed_hydrophobics` query to the orchestrator
//! over the generic raw-bytes dispatch (the reply is opaque proto
//! `ExposedHydrophobicReport` bytes that `App` decodes into per-entity
//! residue refs the viso engine resolves; the flagged residues go to the
//! viso engine, not an orchestrator score merge, so this stays on the
//! generic query path rather than the score-specialized one).
//!
//! Until a plugin advertises the `exposed_hydrophobics` query, this is an
//! inert no-op: [`Self::supports_exposed_hydrophobics`] reports `false` and
//! the caller never requests.

#[cfg(not(target_arch = "wasm32"))]
use super::RunnerClient;

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Whether any plugin has registered the `exposed_hydrophobics` query.
    /// The bridge advertises a query by registration (same index the `score`
    /// query lives in), so this is the host-side support gate: the at-rest
    /// trigger requests the flagged residues ONLY when this is `true`, keeping
    /// the path inert until a plugin implements `exposed_hydrophobics`.
    /// `false` when no orchestrator is installed.
    pub(crate) fn supports_exposed_hydrophobics(&self) -> bool {
        self.orchestrator.as_ref().is_some_and(|orch| {
            orch.plugin_registry()
                .get_query("exposed_hydrophobics")
                .is_some()
        })
    }

    /// Request the `exposed_hydrophobics` query synchronously and return its
    /// raw opaque bytes (proto `ExposedHydrophobicReport`), the payload `App`
    /// decodes into residue refs. Passes no bytes and the default dispatch
    /// context: the query covers the current session pose, like the
    /// whole-assembly `score` query.
    ///
    /// Returns an empty `Vec` (the "none" / "clear" signal) when no
    /// orchestrator is installed, no plugin advertises the query, or the query
    /// errors. The unsupported case is filtered by
    /// [`Self::supports_exposed_hydrophobics`] before the call; the error case
    /// is swallowed at `trace` level so an at-rest miss never spams the log.
    pub(crate) fn request_exposed_hydrophobics_bytes(&mut self) -> Vec<u8> {
        use foldit_runner::orchestrator::DispatchContext;
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        match orch.dispatch_query(
            "exposed_hydrophobics",
            DispatchContext::default(),
            std::collections::HashMap::new(),
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::trace!("[RunnerClient] exposed_hydrophobics query failed: {e}");
                Vec::new()
            }
        }
    }
}
