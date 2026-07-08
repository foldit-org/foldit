//! Runtime environment utilities.
//!
//! Non-pyo3 utilities used by the worker frame: worker binary search and
//! IPC socket naming. Python-specific runtime (PythonConfig,
//! Py_Initialize, libpython resolution) lives in the `foldit-python-host`
//! crate.

pub mod binary;
pub mod socket;

pub use binary::{find_worker_binary, worker_binary_name};
pub use socket::socket_name_for_plugin;
