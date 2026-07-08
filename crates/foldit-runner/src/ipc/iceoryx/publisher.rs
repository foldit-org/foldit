//! Iceoryx publisher for sending large data

use anyhow::Result;
use iceoryx2::node::Node;
use iceoryx2::port::publisher;
use iceoryx2::prelude::*;
use iceoryx2::service::ipc as iceoryx_ipc;
use iceoryx2::service::port_factory::publisher::UnableToDeliverStrategy;

use super::manager::SharedMemoryConfig;

/// Maximum slice length for Iceoryx transfers (10 MB)
/// This should accommodate most protein structure data:
/// - A 1000 residue protein with all heavy atoms = ~7000 atoms
/// - Each atom: 3 floats (x,y,z) = 12 bytes
/// - Total: ~84KB for full atom coordinates
/// - 10MB allows for very large structures with headroom
pub const MAX_SLICE_LEN: usize = 10 * 1024 * 1024;

/// Iceoryx publisher for sending large data
///
/// The Node is stored to ensure proper cleanup when the publisher is dropped.
/// Without storing the Node, shared memory resources may be orphaned.
pub struct IceoryxPublisher {
    #[allow(dead_code)] // Node must be kept alive for proper cleanup
    node: Node<iceoryx_ipc::Service>,
    publisher: publisher::Publisher<iceoryx_ipc::Service, [u8], ()>,
}

impl IceoryxPublisher {
    /// Open (or create) the iceoryx service named by `config` and bind a
    /// publisher port. The created `Node` is retained so dropping the
    /// publisher releases the shared-memory segment.
    ///
    /// # Errors
    ///
    /// Returns an error if the iceoryx node, service, or publisher port
    /// can't be created.
    pub fn new(config: &SharedMemoryConfig) -> Result<Self> {
        let node = NodeBuilder::new().create::<iceoryx_ipc::Service>()?;
        let service_name = config.get_service_name()?;

        let service = node
            .service_builder(&service_name)
            .publish_subscribe::<[u8]>()
            .enable_safe_overflow(true)
            .subscriber_max_buffer_size(10)
            .max_publishers(1)
            .max_subscribers(1)
            .open_or_create()?;

        let publisher = service
            .publisher_builder()
            .max_slice_len(MAX_SLICE_LEN)
            .unable_to_deliver_strategy(UnableToDeliverStrategy::DiscardSample)
            .max_loaned_samples(1)
            .create()?;

        Ok(Self { node, publisher })
    }

    /// Publish `data` as one sample. `data.len()` must not exceed
    /// [`MAX_SLICE_LEN`]; the unable-to-deliver strategy is `Discard`,
    /// so a missing subscriber silently drops the sample.
    ///
    /// # Errors
    ///
    /// Returns an error if the slice loan or send fails (typically out
    /// of shared-memory capacity).
    pub fn send_large_data(&mut self, data: &[u8]) -> Result<()> {
        let sample = self.publisher.loan_slice_uninit(data.len())?;
        let sample = sample.write_from_slice(data);
        let _ = sample.send()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[test]
    #[serial]
    fn test_publisher_creation() {
        let socket_name = format!("test_pub_create_{}", std::process::id());
        let config = SharedMemoryConfig::new(&socket_name);

        let result = IceoryxPublisher::new(&config);
        assert!(
            result.is_ok(),
            "Publisher creation should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_publisher_send_without_subscriber() {
        // When there's no subscriber, send should still succeed (data is
        // dropped) because we use enable_safe_overflow(true)
        let socket_name =
            format!("test_pub_send_no_sub_{}", std::process::id());
        let config = SharedMemoryConfig::new(&socket_name);

        let mut publisher = IceoryxPublisher::new(&config)
            .expect("Publisher creation should succeed");

        let test_data = b"hello world";
        let result = publisher.send_large_data(test_data);
        assert!(
            result.is_ok(),
            "Send should succeed even without subscriber: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_publisher_send_large_data() {
        let socket_name = format!("test_pub_large_{}", std::process::id());
        let config = SharedMemoryConfig::new(&socket_name);

        let mut publisher = IceoryxPublisher::new(&config)
            .expect("Publisher creation should succeed");

        // Simulate typical COORDS data size (~1KB per residue, 100 residues)
        let test_data: Vec<u8> =
            (0..100_000).map(|i| (i % 256) as u8).collect();
        let result = publisher.send_large_data(&test_data);
        assert!(
            result.is_ok(),
            "Large data send should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_publisher_multiple_sends() {
        let socket_name = format!("test_pub_multi_{}", std::process::id());
        let config = SharedMemoryConfig::new(&socket_name);

        let mut publisher = IceoryxPublisher::new(&config)
            .expect("Publisher creation should succeed");

        for i in 0..5 {
            let test_data = format!("message {i}");
            let result = publisher.send_large_data(test_data.as_bytes());
            assert!(
                result.is_ok(),
                "Send {} should succeed: {:?}",
                i,
                result.err()
            );
        }
    }
}
