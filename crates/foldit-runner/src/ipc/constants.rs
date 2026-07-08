//! Protocol constants

/// Signal value in confidence field to indicate Iceoryx usage
/// When confidence == ICEORYX_SIGNAL and coords array is empty,
/// data is available via Iceoryx shared memory
pub const ICEORYX_SIGNAL: f32 = -999.0;
