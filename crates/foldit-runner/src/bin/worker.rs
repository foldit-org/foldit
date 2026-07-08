//! foldit-runner worker process
//!
//! Unified worker that loads the appropriate backend based on environment.
//! Thin wrapper around the worker library.

use anyhow::Result;

fn main() -> Result<()> {
    foldit_runner::worker::main()
}
