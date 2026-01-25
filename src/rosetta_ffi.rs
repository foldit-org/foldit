//! FFI bindings to librosetta_interactive
//!
//! Provides safe Rust wrappers around the C FFI interface for Rosetta operations
//! including pose manipulation, scoring, packing, and minimization.

use std::ffi::{c_char, c_void, CStr, CString};

/// Error codes from rosetta_interactive
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosettaError {
    Success = 0,
    NullPointer = -1,
    InvalidCoords = -2,
    RosettaException = -3,
    NotInitialized = -4,
}

impl RosettaError {
    fn from_code(code: i32) -> Result<(), Self> {
        match code {
            0 => Ok(()),
            -1 => Err(RosettaError::NullPointer),
            -2 => Err(RosettaError::InvalidCoords),
            -3 => Err(RosettaError::RosettaException),
            -4 => Err(RosettaError::NotInitialized),
            _ => Err(RosettaError::RosettaException),
        }
    }
}

// Opaque types from C
#[repr(C)]
pub struct RIPose {
    _private: [u8; 0],
}

#[repr(C)]
pub struct RIScoreFunction {
    _private: [u8; 0],
}

// FFI declarations
#[link(name = "rosetta_interactive")]
extern "C" {
    fn ri_init(database_path: *const c_char) -> i32;
    fn ri_shutdown();

    fn ri_pose_from_coords(coords_data: *const u8, coords_len: usize) -> *mut RIPose;
    fn ri_pose_from_sequence(sequence: *const c_char) -> *mut RIPose;
    fn ri_pose_free(pose: *mut RIPose);
    fn ri_pose_clone(pose: *const RIPose) -> *mut RIPose;

    fn ri_pose_num_residues(pose: *const RIPose) -> usize;
    fn ri_pose_sequence(pose: *const RIPose) -> *mut c_char;
    fn ri_pose_to_coords(pose: *const RIPose, out_len: *mut usize) -> *mut u8;

    fn ri_scorefunction_ref2015() -> *mut RIScoreFunction;
    fn ri_scorefunction_free(sfxn: *mut RIScoreFunction);

    fn ri_score(pose: *mut RIPose, sfxn: *mut RIScoreFunction) -> f64;

    fn ri_pack_rotamers(pose: *mut RIPose, sfxn: *mut RIScoreFunction) -> i32;
    fn ri_pack_rotamers_subset(
        pose: *mut RIPose,
        sfxn: *mut RIScoreFunction,
        residue_indices: *const u32,
        num_residues: usize,
    ) -> i32;

    fn ri_minimize(pose: *mut RIPose, sfxn: *mut RIScoreFunction, max_iterations: u32) -> i32;
    fn ri_minimize_backbone(pose: *mut RIPose, sfxn: *mut RIScoreFunction, max_iterations: u32) -> i32;
    fn ri_minimize_sidechains(pose: *mut RIPose, sfxn: *mut RIScoreFunction, max_iterations: u32) -> i32;

    fn ri_mutate_residue(pose: *mut RIPose, residue_index: u32, new_aa: c_char) -> i32;

    fn ri_idealize(pose: *mut RIPose) -> i32;

    fn ri_free(ptr: *mut c_void);
}

/// Initialize Rosetta with optional database path.
/// Pass None to use default database location.
pub fn init(database_path: Option<&str>) -> Result<(), RosettaError> {
    let result = match database_path {
        Some(path) => {
            let c_path = CString::new(path).map_err(|_| RosettaError::NullPointer)?;
            unsafe { ri_init(c_path.as_ptr()) }
        }
        None => unsafe { ri_init(std::ptr::null()) },
    };
    RosettaError::from_code(result)
}

/// Shutdown Rosetta and free global resources.
pub fn shutdown() {
    unsafe { ri_shutdown() }
}

/// Safe wrapper around a Rosetta Pose
pub struct Pose {
    ptr: *mut RIPose,
}

impl Pose {
    /// Create a pose from COORDS binary format
    pub fn from_coords(coords: &[u8]) -> Option<Self> {
        let ptr = unsafe { ri_pose_from_coords(coords.as_ptr(), coords.len()) };
        if ptr.is_null() {
            None
        } else {
            Some(Pose { ptr })
        }
    }

    /// Create a pose from a sequence string (one-letter amino acid codes)
    pub fn from_sequence(sequence: &str) -> Option<Self> {
        let c_seq = CString::new(sequence).ok()?;
        let ptr = unsafe { ri_pose_from_sequence(c_seq.as_ptr()) };
        if ptr.is_null() {
            None
        } else {
            Some(Pose { ptr })
        }
    }

    /// Clone this pose
    pub fn clone_pose(&self) -> Option<Self> {
        let ptr = unsafe { ri_pose_clone(self.ptr) };
        if ptr.is_null() {
            None
        } else {
            Some(Pose { ptr })
        }
    }

    /// Get the number of residues
    pub fn num_residues(&self) -> usize {
        unsafe { ri_pose_num_residues(self.ptr) }
    }

    /// Get the sequence as a string
    pub fn sequence(&self) -> Option<String> {
        let ptr = unsafe { ri_pose_sequence(self.ptr) };
        if ptr.is_null() {
            return None;
        }
        let result = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { ri_free(ptr as *mut c_void) };
        Some(result)
    }

    /// Export to COORDS binary format
    pub fn to_coords(&self) -> Option<Vec<u8>> {
        let mut len: usize = 0;
        let ptr = unsafe { ri_pose_to_coords(self.ptr, &mut len) };
        if ptr.is_null() || len == 0 {
            return None;
        }
        let result = unsafe { std::slice::from_raw_parts(ptr, len).to_vec() };
        unsafe { ri_free(ptr as *mut c_void) };
        Some(result)
    }

    /// Score this pose
    pub fn score(&mut self, sfxn: &mut ScoreFunction) -> f64 {
        unsafe { ri_score(self.ptr, sfxn.ptr) }
    }

    /// Pack all sidechains (repack rotamers)
    pub fn pack_rotamers(&mut self, sfxn: &mut ScoreFunction) -> Result<(), RosettaError> {
        unsafe { RosettaError::from_code(ri_pack_rotamers(self.ptr, sfxn.ptr)) }
    }

    /// Pack sidechains for specific residues only (1-indexed)
    pub fn pack_rotamers_subset(
        &mut self,
        sfxn: &mut ScoreFunction,
        residue_indices: &[u32],
    ) -> Result<(), RosettaError> {
        unsafe {
            RosettaError::from_code(ri_pack_rotamers_subset(
                self.ptr,
                sfxn.ptr,
                residue_indices.as_ptr(),
                residue_indices.len(),
            ))
        }
    }

    /// Minimize the pose (backbone and sidechains)
    pub fn minimize(&mut self, sfxn: &mut ScoreFunction, max_iterations: u32) -> Result<(), RosettaError> {
        unsafe { RosettaError::from_code(ri_minimize(self.ptr, sfxn.ptr, max_iterations)) }
    }

    /// Minimize backbone only
    pub fn minimize_backbone(&mut self, sfxn: &mut ScoreFunction, max_iterations: u32) -> Result<(), RosettaError> {
        unsafe { RosettaError::from_code(ri_minimize_backbone(self.ptr, sfxn.ptr, max_iterations)) }
    }

    /// Minimize sidechains only
    pub fn minimize_sidechains(&mut self, sfxn: &mut ScoreFunction, max_iterations: u32) -> Result<(), RosettaError> {
        unsafe { RosettaError::from_code(ri_minimize_sidechains(self.ptr, sfxn.ptr, max_iterations)) }
    }

    /// Mutate a residue (1-indexed) to a new amino acid (single letter code)
    pub fn mutate_residue(&mut self, residue_index: u32, new_aa: char) -> Result<(), RosettaError> {
        unsafe { RosettaError::from_code(ri_mutate_residue(self.ptr, residue_index, new_aa as c_char)) }
    }

    /// Idealize the pose backbone geometry
    /// Fixes bond lengths and angles to ideal values while preserving torsion angles.
    /// Essential for structures from ML models (RFD3, etc.) that may have unrealistic geometry.
    pub fn idealize(&mut self) -> Result<(), RosettaError> {
        unsafe { RosettaError::from_code(ri_idealize(self.ptr)) }
    }

    /// Get the raw pointer (for advanced use)
    pub fn as_ptr(&self) -> *const RIPose {
        self.ptr
    }

    /// Get the raw mutable pointer (for advanced use)
    pub fn as_mut_ptr(&mut self) -> *mut RIPose {
        self.ptr
    }
}

impl Drop for Pose {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ri_pose_free(self.ptr) };
        }
    }
}

// Pose is not automatically Send/Sync due to raw pointer
// Uncomment if Rosetta is thread-safe for poses:
// unsafe impl Send for Pose {}

/// Safe wrapper around a Rosetta ScoreFunction
pub struct ScoreFunction {
    ptr: *mut RIScoreFunction,
}

impl ScoreFunction {
    /// Create the ref2015 score function
    pub fn ref2015() -> Option<Self> {
        let ptr = unsafe { ri_scorefunction_ref2015() };
        if ptr.is_null() {
            None
        } else {
            Some(ScoreFunction { ptr })
        }
    }

    /// Get the raw pointer (for advanced use)
    pub fn as_ptr(&self) -> *const RIScoreFunction {
        self.ptr
    }

    /// Get the raw mutable pointer (for advanced use)
    pub fn as_mut_ptr(&mut self) -> *mut RIScoreFunction {
        self.ptr
    }
}

impl Drop for ScoreFunction {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ri_scorefunction_free(self.ptr) };
        }
    }
}

// ScoreFunction is not automatically Send/Sync due to raw pointer
// Uncomment if Rosetta is thread-safe for score functions:
// unsafe impl Send for ScoreFunction {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init() {
        assert!(init(None).is_ok());
        shutdown();
    }
}
