//! Iceoryx shared-memory transport (unwired, forward-looking).
//!
//! A zero-copy shared-memory channel built for moving large binary
//! payloads (e.g. big tensors / coordinate blocks) between the
//! orchestrator and a worker without round-tripping them through the
//! socket. It is NOT currently used by any live code path: the active
//! orchestrator <-> worker IPC is entirely the interprocess local socket
//! in [`crate::ipc::sockets`], with proto messages framed by
//! [`crate::ipc::messaging`].
//!
//! Kept as a ready option for the day large binary payloads over the
//! socket become a bottleneck. Nothing spawns or drives these types yet.

pub mod manager;
pub mod publisher;
pub mod subscriber;

pub use manager::{IceoryxManager, SharedMemoryConfig};
pub use publisher::IceoryxPublisher;
// Subscriber is used internally, not exported directly
