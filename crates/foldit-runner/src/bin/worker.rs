//! foldit-runner worker process
//!
//! Unified worker that loads the appropriate backend based on environment.
//! Thin wrapper around the worker library.

use anyhow::Result;

fn main() -> Result<()> {
    // Rosetta's C++ init uses deep call stacks that exceed Windows' default
    // 1 MB stack. Spawn the real worker on a thread with 8 MB (matching
    // the Unix default) so the native plugin doesn't overflow.
    let builder = std::thread::Builder::new()
        .name("worker-main".into())
        .stack_size(8 * 1024 * 1024);
    let handle = builder.spawn(|| foldit_runner::worker::main())?;
    handle.join().unwrap_or_else(|e| std::panic::resume_unwind(e))
}
