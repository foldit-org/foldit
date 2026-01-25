/**
 * rosetta_interactive.h
 *
 * C FFI interface for Rosetta operations needed for interactive protein manipulation.
 * Provides pose manipulation, scoring, packing, and minimization.
 */

#ifndef ROSETTA_INTERACTIVE_H
#define ROSETTA_INTERACTIVE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a Rosetta Pose */
typedef struct RIPose RIPose;

/* Opaque handle to a Rosetta ScoreFunction */
typedef struct RIScoreFunction RIScoreFunction;

/* Error codes */
typedef enum {
    RI_SUCCESS = 0,
    RI_ERROR_NULL_POINTER = -1,
    RI_ERROR_INVALID_COORDS = -2,
    RI_ERROR_ROSETTA_EXCEPTION = -3,
    RI_ERROR_NOT_INITIALIZED = -4,
} RIError;

/* ============================================================================
 * Initialization
 * ============================================================================ */

/**
 * Initialize Rosetta with database path.
 * database_path: Path to Rosetta database directory (NULL for default)
 * Returns RI_SUCCESS on success.
 */
int ri_init(const char* database_path);

/**
 * Shutdown Rosetta and free global resources.
 */
void ri_shutdown(void);

/* ============================================================================
 * Pose Creation and Destruction
 * ============================================================================ */

/**
 * Create a pose from COORDS binary format.
 * Returns NULL on failure.
 */
RIPose* ri_pose_from_coords(const uint8_t* coords_data, size_t coords_len);

/**
 * Create a pose from a sequence string (one-letter amino acid codes).
 * Returns NULL on failure.
 */
RIPose* ri_pose_from_sequence(const char* sequence);

/**
 * Create a pose from a PDB format string.
 * Returns NULL on failure.
 */
RIPose* ri_pose_from_pdb_string(const char* pdb_string);

/**
 * Free a pose.
 */
void ri_pose_free(RIPose* pose);

/**
 * Clone a pose.
 * Returns NULL on failure.
 */
RIPose* ri_pose_clone(const RIPose* pose);

/* ============================================================================
 * Pose Queries
 * ============================================================================ */

/**
 * Get the number of residues in the pose.
 */
size_t ri_pose_num_residues(const RIPose* pose);

/**
 * Get the sequence as a null-terminated string.
 * Caller must free the returned string with ri_free().
 */
char* ri_pose_sequence(const RIPose* pose);

/**
 * Export pose to COORDS binary format.
 * Sets *out_len to the length of the returned buffer.
 * Caller must free the returned buffer with ri_free().
 * Returns NULL on failure.
 */
uint8_t* ri_pose_to_coords(const RIPose* pose, size_t* out_len);

/* ============================================================================
 * Scoring
 * ============================================================================ */

/**
 * Create the ref2015 score function.
 * Returns NULL on failure.
 */
RIScoreFunction* ri_scorefunction_ref2015(void);

/**
 * Free a score function.
 */
void ri_scorefunction_free(RIScoreFunction* sfxn);

/**
 * Score a pose. Returns the total score.
 */
double ri_score(RIPose* pose, RIScoreFunction* sfxn);

/* ============================================================================
 * Packing (Sidechain Optimization)
 * ============================================================================ */

/**
 * Pack all sidechains in the pose (repack rotamers).
 * Returns RI_SUCCESS on success.
 */
int ri_pack_rotamers(RIPose* pose, RIScoreFunction* sfxn);

/**
 * Pack sidechains for specific residues only.
 * residue_indices is 1-indexed (Rosetta convention).
 * Returns RI_SUCCESS on success.
 */
int ri_pack_rotamers_subset(
    RIPose* pose,
    RIScoreFunction* sfxn,
    const uint32_t* residue_indices,
    size_t num_residues
);

/* ============================================================================
 * Minimization
 * ============================================================================ */

/**
 * Minimize the pose (backbone and sidechains).
 * max_iterations: maximum number of minimization iterations (0 = default).
 * Returns RI_SUCCESS on success.
 */
int ri_minimize(RIPose* pose, RIScoreFunction* sfxn, uint32_t max_iterations);

/**
 * Minimize backbone only.
 */
int ri_minimize_backbone(RIPose* pose, RIScoreFunction* sfxn, uint32_t max_iterations);

/**
 * Minimize sidechains only.
 */
int ri_minimize_sidechains(RIPose* pose, RIScoreFunction* sfxn, uint32_t max_iterations);

/* ============================================================================
 * Mutation
 * ============================================================================ */

/**
 * Mutate a residue to a new amino acid type.
 * residue_index is 1-indexed.
 * new_aa is single-letter amino acid code (e.g., 'A' for alanine).
 * Returns RI_SUCCESS on success.
 */
int ri_mutate_residue(RIPose* pose, uint32_t residue_index, char new_aa);

/* ============================================================================
 * Idealization
 * ============================================================================ */

/**
 * Idealize the pose backbone geometry.
 * Fixes bond lengths and angles to ideal values while preserving torsion angles.
 * Essential for structures from ML models (RFD3, etc.) that may have unrealistic geometry.
 * Returns RI_SUCCESS on success.
 */
int ri_idealize(RIPose* pose);

/* ============================================================================
 * Memory Management
 * ============================================================================ */

/**
 * Free memory allocated by ri_* functions.
 */
void ri_free(void* ptr);

#ifdef __cplusplus
}
#endif

#endif /* ROSETTA_INTERACTIVE_H */
