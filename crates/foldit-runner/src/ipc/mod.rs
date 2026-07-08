//! IPC protocol implementation
//!
//! The live orchestrator <-> worker path is entirely the interprocess
//! local socket in [`sockets`], carrying length-prefixed proto messages
//! ([`messaging`]). The [`iceoryx`] shared-memory transport is an unwired,
//! forward-looking option (see its module docs); it is not on any live
//! code path.

pub mod constants;
pub mod iceoryx;
pub mod messaging;
pub mod sockets;

// Re-export commonly used items
pub use constants::ICEORYX_SIGNAL;
pub use iceoryx::*;
pub use messaging::*;
pub use sockets::*;
