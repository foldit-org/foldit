//! Python runtime configuration and detection
//!
//! Modified version of original `python_loader/config.rs`
//! Now takes environment name instead of model name.
use std::collections::HashSet;
use std::env;
use std::path::PathBuf;

use anyhow::Result;
use log;

use crate::library;

#[derive(Debug, Clone)]
pub struct PythonConfig {
    pub python_home: PathBuf,
    pub python_paths: Vec<PathBuf>,
    pub env_name: String,
}

impl PythonConfig {
    /// Resolve the Python environment for `env_name`.
    ///
    /// The orchestrator activates the env before spawning the worker by
    /// setting `PYTHONHOME` to the env directory (bundle:
    /// `<plugin_dir>/env`; dev: `.pixi/envs/<env>`), so that is the
    /// primary path. As a fallback for a worker launched by hand without
    /// `PYTHONHOME`, scan the dev tree for `.pixi/envs/<env_name>`. pixi
    /// is never invoked here.
    ///
    /// # Errors
    ///
    /// Returns `Err` if no Python environment can be resolved from
    /// `PYTHONHOME` or the dev `.pixi/envs/<env_name>` tree.
    pub fn auto_detect(env_name: &str) -> Result<Self> {
        log::info!("Resolving Python environment for '{env_name}'");
        log::info!("  Current dir: {:?}", env::current_dir().ok());
        log::info!("  Executable: {:?}", env::current_exe().ok());

        // Primary: PYTHONHOME, set by the orchestrator at spawn time.
        if let Some(config) = Self::from_pythonhome(env_name) {
            log::info!("Using PYTHONHOME for env '{env_name}'");
            return Ok(config);
        }

        // Fallback: a worker launched by hand outside the orchestrator —
        // scan the dev tree for .pixi/envs/<env_name>.
        if let Some(config) = Self::try_dev_pixi_env(env_name) {
            log::info!("Found dev pixi env for '{env_name}'");
            return Ok(config);
        }

        anyhow::bail!(
            "No Python environment found for env '{env_name}'.\n\nExpected \
             PYTHONHOME to point at the env (the orchestrator sets it at \
             spawn time), or a dev env at \
             <runner_root>/.pixi/envs/{env_name}.\n\nRun 'cargo xtask \
             setup-envs' from the workspace root to create the dev \
             environments.",
        )
    }

    /// Build a config from `PYTHONHOME` if it is set and looks like a real
    /// Python home. This is the path the orchestrator drives.
    fn from_pythonhome(env_name: &str) -> Option<Self> {
        let python_home = env::var_os("PYTHONHOME").map(PathBuf::from)?;
        if !library::is_valid_python_home(&python_home) {
            log::warn!(
                "PYTHONHOME = {} but it is not a valid Python home (no \
                 libpython); ignoring",
                python_home.display()
            );
            return None;
        }
        log::info!("PYTHONHOME = {}", python_home.display());
        Some(Self {
            python_home,
            python_paths: vec![],
            env_name: env_name.to_owned(),
        })
    }

    /// Fallback dev resolver: walk candidate roots looking for
    /// `.pixi/envs/<env_name>`. Only used when a worker is launched by
    /// hand without `PYTHONHOME`.
    fn try_dev_pixi_env(env_name: &str) -> Option<Self> {
        let mut candidates = Vec::new();

        // 1. From project root (found via cwd + pixi.toml)
        if let Ok(project_root) = Self::find_project_root() {
            candidates.push(project_root.clone());
            candidates.push(project_root.join("crates").join("foldit-runner"));
        }

        // 2. From executable location (target/<profile>/foldit-worker ->
        //    ../../crates/foldit-runner)
        if let Ok(exe) = env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let workspace_root = exe_dir.join("..").join("..");
                if let Ok(canonical_root) = workspace_root.canonicalize() {
                    candidates.push(
                        canonical_root.join("crates").join("foldit-runner"),
                    );
                    candidates.push(canonical_root);
                } else {
                    candidates.push(
                        workspace_root.join("crates").join("foldit-runner"),
                    );
                    candidates.push(workspace_root);
                }
            }
        }

        // Deduplicate candidates
        let mut seen = HashSet::new();
        candidates.retain(|p| {
            let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
            seen.insert(canonical)
        });

        for pixi_root in &candidates {
            let python_home =
                pixi_root.join(".pixi").join("envs").join(env_name);
            if python_home.exists() {
                log::info!(
                    "Found pixi environment at: {}",
                    python_home.display()
                );
                return Some(Self {
                    python_home,
                    python_paths: vec![],
                    env_name: env_name.to_owned(),
                });
            }
        }

        log::warn!(
            "Pixi environment '{env_name}' not found in any candidate location"
        );
        None
    }

    /// Find the project root by looking for pixi.toml
    fn find_project_root() -> Result<PathBuf> {
        // Try PIXI_PROJECT_ROOT environment variable first
        if let Ok(project_root) = env::var("PIXI_PROJECT_ROOT") {
            let path = PathBuf::from(project_root);
            if path.join("pixi.toml").exists() {
                return Ok(path);
            }
        }

        // Walk up from current directory to find pixi.toml
        let mut current = env::current_dir()?;
        loop {
            if current.join("pixi.toml").exists() {
                return Ok(current);
            }
            if !current.pop() {
                break;
            }
        }

        // Fallback to current directory if no pixi.toml found
        Ok(env::current_dir()?)
    }
}
