//! Tee logger: writes log records to stderr AND a shared ring buffer.
//!
//! Replaces `env_logger` so that log output can be pushed to the webview debug panel.

use log::{Log, Metadata, Record};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Maximum number of log lines retained in the buffer.
const MAX_LINES: usize = 200;

pub type LogBuffer = Arc<Mutex<VecDeque<String>>>;

struct TeeLogger {
    buffer: LogBuffer,
    filter: env_filter::Filter,
}

impl Log for TeeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.filter.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.filter.matches(record) {
            return;
        }
        let line = format!("{} {}: {}", record.level(), record.target(), record.args());

        // Write to stderr
        eprintln!("{}", line);

        // Append to ring buffer
        if let Ok(mut buf) = self.buffer.lock() {
            if buf.len() >= MAX_LINES {
                buf.pop_front();
            }
            buf.push_back(line);
        }
    }

    fn flush(&self) {}
}

/// Initialize the tee logger as the global logger.
/// Returns the shared log buffer for draining into the frontend.
pub fn init(filter_str: &str) -> LogBuffer {
    let buffer: LogBuffer = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LINES)));

    let filter = env_filter::Builder::new()
        .parse(filter_str)
        .build();

    let max_level = filter.filter();

    let logger = TeeLogger {
        buffer: Arc::clone(&buffer),
        filter,
    };

    log::set_boxed_logger(Box::new(logger)).expect("Failed to set logger");
    log::set_max_level(max_level);

    buffer
}
