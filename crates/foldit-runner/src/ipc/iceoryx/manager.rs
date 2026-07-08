//! Iceoryx manager that runs subscribers in a dedicated thread

use std::collections::HashMap;
use std::convert::TryInto;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use iceoryx2::node::Node;
use iceoryx2::port::subscriber;
use iceoryx2::prelude::*;
use iceoryx2::service::ipc as iceoryx_ipc;
use iceoryx2::service::service_name::ServiceName;

/// Holds a subscriber and its associated node for proper cleanup
struct SubscriberWithNode {
    #[allow(dead_code)] // Node must be kept alive for proper cleanup
    node: Node<iceoryx_ipc::Service>,
    subscriber: subscriber::Subscriber<iceoryx_ipc::Service, [u8], ()>,
}

/// Iceoryx shared memory configuration
pub struct SharedMemoryConfig {
    socket_name: String,
}

impl SharedMemoryConfig {
    /// Build a config keyed on `socket_name`; the service name is
    /// derived deterministically by hashing.
    #[must_use]
    pub fn new(socket_name: &str) -> Self {
        Self {
            socket_name: String::from(socket_name),
        }
    }

    fn service_name(&self) -> Result<ServiceName> {
        // Create a hash of socket name to avoid invalid characters and length
        // issues
        let mut hasher = DefaultHasher::new();
        self.socket_name.hash(&mut hasher);
        let hash = hasher.finish();
        let service_name_str = format!("foldit_runner/large_data/{hash:x}");
        service_name_str
            .as_str()
            .try_into()
            .map_err(|e| anyhow!("Invalid service name: {e}"))
    }

    /// Resolve the iceoryx service name for this socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the derived service name is not a valid
    /// iceoryx `ServiceName`.
    pub fn get_service_name(&self) -> Result<ServiceName> {
        self.service_name()
    }
}

/// Iceoryx manager that runs subscribers in a dedicated thread
pub struct IceoryxManager {
    request_tx: mpsc::Sender<IceoryxRequest>,
}

/// Request sent to the Iceoryx thread
enum IceoryxRequest {
    RegisterService(String),
    GetData(String, mpsc::Sender<IceoryxResponse>),
    Shutdown,
}

/// Response from the Iceoryx thread
enum IceoryxResponse {
    Data(Option<Vec<u8>>),
    Error(String),
}

fn handle_register(
    subscribers: &mut HashMap<String, SubscriberWithNode>,
    socket_name: String,
) {
    let config = SharedMemoryConfig::new(&socket_name);
    let service_name = match config.service_name() {
        Ok(name) => name,
        Err(e) => {
            log::warn!("Failed to get service name for {socket_name}: {e}");
            return;
        }
    };
    match IceoryxManager::create_subscriber(&service_name) {
        Ok(subscriber_with_node) => {
            let _ = subscribers.insert(socket_name, subscriber_with_node);
        }
        Err(e) => {
            log::warn!(
                "Failed to create Iceoryx subscriber for {socket_name}: {e}"
            );
        }
    }
}

fn handle_get_data(
    subscribers: &HashMap<String, SubscriberWithNode>,
    socket_name: &str,
    response_tx: &mpsc::Sender<IceoryxResponse>,
) {
    let result = subscribers.get(socket_name).map_or_else(
        || {
            IceoryxResponse::Error(format!(
                "No subscriber found for socket: {socket_name}"
            ))
        },
        |sub_with_node| match sub_with_node.subscriber.receive() {
            Ok(Some(sample)) => {
                IceoryxResponse::Data(Some(sample.payload().to_vec()))
            }
            Ok(None) => IceoryxResponse::Data(None),
            Err(e) => {
                IceoryxResponse::Error(format!("Failed to receive data: {e}"))
            }
        },
    );
    if response_tx.send(result).is_err() {
        log::warn!("Failed to send Iceoryx response, receiver dropped");
    }
}

impl IceoryxManager {
    /// Create a new `IceoryxManager` with a dedicated thread.
    ///
    /// # Errors
    ///
    /// Currently infallible; the `Result` is reserved for future
    /// background-thread setup that may fail.
    pub fn new() -> Result<Self> {
        let (request_tx, request_rx) = mpsc::channel();

        let _ = thread::spawn(move || {
            let mut subscribers: HashMap<String, SubscriberWithNode> =
                HashMap::new();

            for request in request_rx {
                match request {
                    IceoryxRequest::RegisterService(socket_name) => {
                        handle_register(&mut subscribers, socket_name);
                    }
                    IceoryxRequest::GetData(socket_name, response_tx) => {
                        handle_get_data(
                            &subscribers,
                            &socket_name,
                            &response_tx,
                        );
                    }
                    IceoryxRequest::Shutdown => break,
                }
            }

            subscribers.clear();
        });

        Ok(Self { request_tx })
    }

    /// Create an Iceoryx subscriber for a service
    ///
    /// Returns both the subscriber and its associated node to ensure proper
    /// cleanup.
    fn create_subscriber(
        service_name: &ServiceName,
    ) -> Result<SubscriberWithNode> {
        let node = NodeBuilder::new().create::<iceoryx_ipc::Service>()?;

        let service = node
            .service_builder(service_name)
            .publish_subscribe::<[u8]>()
            .enable_safe_overflow(true)
            .subscriber_max_buffer_size(10)
            .max_publishers(1)
            .max_subscribers(1)
            .open_or_create()?;

        let subscriber = service.subscriber_builder().create()?;
        Ok(SubscriberWithNode { node, subscriber })
    }

    /// Register a service for Iceoryx communication.
    ///
    /// # Errors
    ///
    /// Returns an error if the request channel to the background thread
    /// is disconnected.
    pub fn register_service(&self, socket_name: &str) -> Result<()> {
        self.request_tx
            .send(IceoryxRequest::RegisterService(String::from(socket_name)))
            .map_err(|e| anyhow!("Failed to send register request: {e}"))?;
        Ok(())
    }

    /// Get large data from Iceoryx with a timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the background thread reports a failure or
    /// disconnects before responding.
    pub fn get_large_data_timeout(
        &self,
        socket_name: &str,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        let (response_tx, response_rx) = mpsc::channel();

        self.request_tx
            .send(IceoryxRequest::GetData(
                String::from(socket_name),
                response_tx,
            ))
            .map_err(|e| anyhow!("Failed to send get data request: {e}"))?;

        match response_rx.recv_timeout(timeout) {
            Ok(IceoryxResponse::Data(data)) => Ok(data),
            Ok(IceoryxResponse::Error(err)) => {
                Err(anyhow!("Iceoryx error: {err}"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("Iceoryx thread disconnected"))
            }
        }
    }

    /// Shut down the Iceoryx thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the request channel is already closed.
    pub fn shutdown(&self) -> Result<()> {
        self.request_tx
            .send(IceoryxRequest::Shutdown)
            .map_err(|e| anyhow!("Failed to send shutdown request: {e}"))?;
        Ok(())
    }
}

impl Drop for IceoryxManager {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[test]
    fn test_shared_memory_config_service_name() {
        let config = SharedMemoryConfig::new("test_socket");
        let result = config.get_service_name();
        assert!(
            result.is_ok(),
            "Service name generation should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_shared_memory_config_deterministic() {
        let config1 = SharedMemoryConfig::new("test_socket_123");
        let config2 = SharedMemoryConfig::new("test_socket_123");

        let name1 = config1.get_service_name().unwrap();
        let name2 = config2.get_service_name().unwrap();

        assert_eq!(
            format!("{name1:?}"),
            format!("{name2:?}"),
            "Same socket name should produce same service name"
        );
    }

    #[test]
    fn test_shared_memory_config_different_sockets() {
        let config1 = SharedMemoryConfig::new("socket_a");
        let config2 = SharedMemoryConfig::new("socket_b");

        let name1 = config1.get_service_name().unwrap();
        let name2 = config2.get_service_name().unwrap();

        assert_ne!(
            format!("{name1:?}"),
            format!("{name2:?}"),
            "Different socket names should produce different service names"
        );
    }

    #[test]
    #[serial]
    fn test_manager_creation() {
        let result = IceoryxManager::new();
        assert!(
            result.is_ok(),
            "Manager creation should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_manager_register_service() {
        let manager =
            IceoryxManager::new().expect("Manager creation should succeed");
        let socket_name = format!("test_mgr_reg_{}", std::process::id());

        let result = manager.register_service(&socket_name);
        assert!(
            result.is_ok(),
            "Register service should succeed: {:?}",
            result.err()
        );

        // Give the background thread time to process
        thread::sleep(Duration::from_millis(50));
    }

    #[test]
    #[serial]
    fn test_manager_get_data_unregistered_socket() {
        let manager =
            IceoryxManager::new().expect("Manager creation should succeed");
        let socket_name = format!("test_mgr_unreg_{}", std::process::id());

        let result = manager
            .get_large_data_timeout(&socket_name, Duration::from_millis(100));
        assert!(
            result.is_err(),
            "Get data from unregistered socket should error"
        );
    }

    #[test]
    #[serial]
    fn test_manager_get_data_no_data() {
        let manager =
            IceoryxManager::new().expect("Manager creation should succeed");
        let socket_name = format!("test_mgr_nodata_{}", std::process::id());

        manager
            .register_service(&socket_name)
            .expect("Register should succeed");

        // Give the background thread time to process registration
        thread::sleep(Duration::from_millis(100));

        let result = manager
            .get_large_data_timeout(&socket_name, Duration::from_millis(100));
        assert!(
            result.is_ok(),
            "Get data should succeed (with None): {:?}",
            result.err()
        );
        assert!(
            result.unwrap().is_none(),
            "Should return None when no data available"
        );
    }

    #[test]
    #[serial]
    fn test_manager_shutdown() {
        let manager =
            IceoryxManager::new().expect("Manager creation should succeed");
        let result = manager.shutdown();
        assert!(
            result.is_ok(),
            "Shutdown should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_full_pub_sub_roundtrip() {
        use super::super::publisher::IceoryxPublisher;

        let socket_name = format!("test_roundtrip_{}", std::process::id());
        let config = SharedMemoryConfig::new(&socket_name);

        // Create manager and register service (subscriber side)
        let manager =
            IceoryxManager::new().expect("Manager creation should succeed");
        manager
            .register_service(&socket_name)
            .expect("Register should succeed");

        // Give the background thread time to create the subscriber
        thread::sleep(Duration::from_millis(100));

        // Create publisher (worker side)
        let mut publisher = IceoryxPublisher::new(&config)
            .expect("Publisher creation should succeed");

        let test_data = b"hello iceoryx roundtrip test!";
        publisher
            .send_large_data(test_data)
            .expect("Send should succeed");

        // Give time for the message to be available
        thread::sleep(Duration::from_millis(50));

        let result = manager
            .get_large_data_timeout(&socket_name, Duration::from_secs(1));
        assert!(
            result.is_ok(),
            "Get data should succeed: {:?}",
            result.err()
        );

        let received = result.unwrap();
        assert!(received.is_some(), "Should receive data");
        assert_eq!(
            received.unwrap(),
            test_data,
            "Received data should match sent data"
        );
    }

    #[test]
    #[serial]
    fn test_full_pub_sub_large_data() {
        use super::super::publisher::IceoryxPublisher;

        let socket_name =
            format!("test_large_roundtrip_{}", std::process::id());
        let config = SharedMemoryConfig::new(&socket_name);

        let manager =
            IceoryxManager::new().expect("Manager creation should succeed");
        manager
            .register_service(&socket_name)
            .expect("Register should succeed");
        thread::sleep(Duration::from_millis(100));

        let mut publisher = IceoryxPublisher::new(&config)
            .expect("Publisher creation should succeed");

        // Send large test data (100KB)
        let test_data: Vec<u8> =
            (0..100_000).map(|i| (i % 256) as u8).collect();
        publisher
            .send_large_data(&test_data)
            .expect("Send should succeed");

        thread::sleep(Duration::from_millis(50));

        let result = manager
            .get_large_data_timeout(&socket_name, Duration::from_secs(1));
        assert!(
            result.is_ok(),
            "Get data should succeed: {:?}",
            result.err()
        );

        let received = result.unwrap();
        assert!(received.is_some(), "Should receive data");
        assert_eq!(received.unwrap(), test_data, "Large data should match");
    }
}
