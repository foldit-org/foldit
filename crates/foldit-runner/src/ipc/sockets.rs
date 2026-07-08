//! Socket creation and connection utilities

use std::io;

use anyhow::{Context, Result};
use interprocess::local_socket::prelude::*;
pub use interprocess::local_socket::prelude::{
    LocalSocketListener, LocalSocketStream,
};
use interprocess::local_socket::{
    GenericNamespaced, ListenerNonblockingMode, ListenerOptions,
};

/// Create a local socket listener with a unique name for a plugin.
///
/// Returns the listener and the socket name to pass to workers.
///
/// # Errors
///
/// Returns an error if the socket name conversion or listener bind
/// fails.
pub fn create_listener(
    plugin_id: &str,
) -> Result<(LocalSocketListener, String)> {
    // Get platform-specific socket name
    let socket_name = crate::runtime::socket_name_for_plugin(plugin_id);

    // Create listener
    let listener = ListenerOptions::new()
        .name(socket_name.clone().to_ns_name::<GenericNamespaced>()?)
        .create_sync()
        .context("failed to create local socket listener")?;

    Ok((listener, socket_name))
}

/// Accept a connection from a listener.
///
/// # Errors
///
/// Returns an error if the accept call fails.
pub fn accept_connection(
    listener: &LocalSocketListener,
) -> Result<LocalSocketStream> {
    listener
        .accept()
        .context("failed to accept connection from worker")
}

/// Put a listener into non-blocking accept mode.
///
/// A later [`try_accept_connection`] then returns immediately instead of
/// blocking until a worker connects. This affects only the `accept` call.
/// The accepted stream must still block on its round-trips (the pump thread
/// blocks on each write/read), so [`try_accept_connection`] explicitly forces
/// the accepted stream back to blocking: on some platforms (macOS) `accept`
/// propagates the listener's non-blocking file-status flag onto the new
/// socket, and the `interprocess` `Accept` mode does not clear it.
///
/// # Errors
///
/// Returns an error if the platform rejects the mode change.
pub fn set_accept_nonblocking(listener: &LocalSocketListener) -> Result<()> {
    listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .context("failed to set listener to non-blocking accept")
}

/// Try to accept a worker connection without blocking.
///
/// Returns `Ok(Some(stream))` when a worker has connected, `Ok(None)`
/// when none is waiting yet (the caller polls again next frame), or an
/// error on a genuine accept failure. Requires the listener to have been
/// put into non-blocking accept mode via [`set_accept_nonblocking`].
///
/// # Errors
///
/// Returns an error if the accept call fails for a reason other than
/// "no connection waiting".
pub fn try_accept_connection(
    listener: &LocalSocketListener,
) -> Result<Option<LocalSocketStream>> {
    match listener.accept() {
        Ok(stream) => {
            // Force the accepted stream back to blocking. On macOS, accept
            // propagates the listener's non-blocking flag onto the new socket
            // and the interprocess Accept mode leaves it set; the pump thread
            // must block on its round-trips, so clear it explicitly here.
            stream
                .set_nonblocking(false)
                .context("failed to set accepted stream to blocking")?;
            Ok(Some(stream))
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e).context("failed to accept connection from worker"),
    }
}

/// Connect to a local socket as a client.
///
/// # Errors
///
/// Returns an error if `socket_name` is invalid or the connect fails.
pub fn connect_to_socket(socket_name: &str) -> Result<LocalSocketStream> {
    LocalSocketStream::connect(socket_name.to_ns_name::<GenericNamespaced>()?)
        .with_context(|| format!("failed to connect to socket: {socket_name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn test_create_unix_socket() {
        // Note: create_listener uses /tmp for socket files, so no need to
        // change cwd
        let (listener, socket_name) = create_listener("test").unwrap();
        assert!(socket_name.contains("/tmp/foldit-runner-test-"));
        drop(listener); // Cleanup
    }
}
