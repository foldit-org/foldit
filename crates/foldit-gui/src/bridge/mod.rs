//! Transport-agnostic JS↔Rust bridge.
//!
//! Both desktop (wry) and web (wasm-bindgen) builds share the message
//! shapes, JSON decoder, dispatch trait, and dirty-section serializer.
//! Only the byte-delivery channel ([`Transport`]) varies per platform.

pub mod decode;
pub mod dispatch;
pub mod message;
pub mod push;
pub mod transport;

pub use dispatch::Dispatcher;
pub use message::{IpcMessage, RequestKind};
pub use transport::{RequestResult, Transport};
