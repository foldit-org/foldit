/**
 * rosetta_interactive.cpp
 *
 * Implementation of the C FFI interface for Rosetta.
 */

#include "rosetta_interactive.h"

// Rosetta headers
#include <core/init/init.hh>
#include <core/pose/Pose.hh>
#include <core/pose/annotated_sequence.hh>
#include <core/io/pdb/pdb_writer.hh>
#include <core/scoring/ScoreFunction.hh>
#include <core/scoring/ScoreFunctionFactory.hh>
#include <core/scoring/ScoreType.hh>
#include <core/pack/pack_rotamers.hh>
#include <core/pack/task/TaskFactory.hh>
#include <core/pack/task/PackerTask.hh>
#include <core/pack/task/operation/TaskOperations.hh>
#include <core/pack/task/ResidueLevelTask.hh>
#include <core/kinematics/MoveMap.hh>
#include <core/optimization/AtomTreeMinimizer.hh>
#include <core/optimization/CartesianMinimizer.hh>
#include <core/optimization/MinimizerOptions.hh>
#include <core/conformation/Residue.hh>
#include <core/chemical/ResidueType.hh>
#include <core/chemical/ChemicalManager.hh>
#include <core/chemical/AA.hh>
#include <core/id/AtomID.hh>

#include <core/pose/util.hh>
#include <core/conformation/ResidueFactory.hh>
#include <core/conformation/util.hh>

#include <basic/options/option.hh>

#include <utility/pointer/owning_ptr.hh>

#include <string>
#include <sstream>
#include <cstring>
#include <vector>
#include <set>

// Internal wrapper structs
struct RIPose {
    core::pose::PoseOP pose;
    // Track which atoms were set (for export - only export what we imported)
    std::set<core::id::AtomID> set_atoms;
};

struct RIScoreFunction {
    core::scoring::ScoreFunctionOP sfxn;
};

// Global state
static bool g_initialized = false;

/* ============================================================================
 * Initialization
 * ============================================================================ */

extern "C" int ri_init(const char* database_path) {
    if (g_initialized) {
        return RI_SUCCESS;
    }

    try {
        // Build argv with database path if provided
        std::vector<char*> argv_vec;
        argv_vec.push_back((char*)"rosetta_interactive");

        std::string db_arg;
        if (database_path && database_path[0] != '\0') {
            db_arg = std::string("-database=") + database_path;
            argv_vec.push_back(const_cast<char*>(db_arg.c_str()));
        }
        argv_vec.push_back(nullptr);

        int argc = static_cast<int>(argv_vec.size()) - 1;
        core::init::init(argc, argv_vec.data());
        g_initialized = true;
        return RI_SUCCESS;
    } catch (const std::exception& e) {
        return RI_ERROR_ROSETTA_EXCEPTION;
    }
}

extern "C" void ri_shutdown(void) {
    // Rosetta doesn't have a formal shutdown, but we can mark as uninitialized
    g_initialized = false;
}

/* ============================================================================
 * Pose Creation and Destruction
 * ============================================================================ */

// Helper to read big-endian values from buffer
static uint32_t read_u32_be(const uint8_t* data) {
    return (uint32_t(data[0]) << 24) | (uint32_t(data[1]) << 16) |
           (uint32_t(data[2]) << 8) | uint32_t(data[3]);
}

static int32_t read_i32_be(const uint8_t* data) {
    return static_cast<int32_t>(read_u32_be(data));
}

static float read_f32_be(const uint8_t* data) {
    uint32_t bits = read_u32_be(data);
    float result;
    std::memcpy(&result, &bits, sizeof(float));
    return result;
}

extern "C" RIPose* ri_pose_from_coords(const uint8_t* coords_data, size_t coords_len) {
    if (!g_initialized) return nullptr;
    if (!coords_data || coords_len < 12) return nullptr;

    try {
        // Verify magic header "COORDS00"
        if (std::memcmp(coords_data, "COORDS00", 8) != 0) {
            return nullptr;
        }

        uint32_t num_atoms = read_u32_be(coords_data + 8);
        size_t expected_size = 12 + num_atoms * 24;
        if (coords_len < expected_size) {
            return nullptr;
        }

        // Parse atoms and group by residue
        struct AtomData {
            float x, y, z;
            std::string atom_name;
        };
        struct ResidueData {
            std::string res_name;
            int32_t res_num;
            uint8_t chain_id;
            std::vector<AtomData> atoms;
        };

        std::vector<ResidueData> residues;
        int32_t last_res_num = -999999;
        uint8_t last_chain_id = 0;

        const uint8_t* ptr = coords_data + 12;
        for (uint32_t i = 0; i < num_atoms; ++i) {
            float x = read_f32_be(ptr); ptr += 4;
            float y = read_f32_be(ptr); ptr += 4;
            float z = read_f32_be(ptr); ptr += 4;
            uint8_t chain_id = *ptr; ptr += 1;
            std::string res_name(reinterpret_cast<const char*>(ptr), 3); ptr += 3;
            int32_t res_num = read_i32_be(ptr); ptr += 4;
            std::string atom_name(reinterpret_cast<const char*>(ptr), 4); ptr += 4;

            // Trim whitespace from atom name
            while (!atom_name.empty() && atom_name.back() == ' ') atom_name.pop_back();
            while (!atom_name.empty() && atom_name.front() == ' ') atom_name.erase(0, 1);

            // New residue?
            if (res_num != last_res_num || chain_id != last_chain_id) {
                residues.push_back({res_name, res_num, chain_id, {}});
                last_res_num = res_num;
                last_chain_id = chain_id;
            }

            residues.back().atoms.push_back({x, y, z, atom_name});
        }

        // Build sequence from residue names
        std::string sequence;
        for (const auto& res : residues) {
            // Convert 3-letter code to 1-letter
            core::chemical::AA aa = core::chemical::aa_from_name(res.res_name);
            if (aa == core::chemical::aa_unk) {
                // Unknown residue, skip or use X
                sequence += 'X';
            } else {
                sequence += core::chemical::oneletter_code_from_aa(aa);
            }
        }

        if (sequence.empty()) {
            return nullptr;
        }

        // Create pose from sequence
        auto* wrapper = new RIPose();
        wrapper->pose = utility::pointer::make_shared<core::pose::Pose>();
        core::pose::make_pose_from_sequence(*wrapper->pose, sequence,
            *core::chemical::ChemicalManager::get_instance()->residue_type_set(core::chemical::FA_STANDARD));

        // Set atom coordinates using batch operation for proper internal coordinate update
        utility::vector1<core::id::AtomID> atom_ids;
        utility::vector1<core::Vector> coords;

        int matched = 0, unmatched = 0;
        for (size_t res_idx = 0; res_idx < residues.size(); ++res_idx) {
            const auto& res_data = residues[res_idx];
            core::Size rosetta_resnum = res_idx + 1; // Rosetta is 1-indexed

            for (const auto& atom_data : res_data.atoms) {
                // Try to find the atom in the Rosetta residue
                if (wrapper->pose->residue(rosetta_resnum).has(atom_data.atom_name)) {
                    core::id::AtomID atom_id(
                        wrapper->pose->residue(rosetta_resnum).atom_index(atom_data.atom_name),
                        rosetta_resnum
                    );
                    atom_ids.push_back(atom_id);
                    coords.push_back(core::Vector(atom_data.x, atom_data.y, atom_data.z));
                    matched++;
                } else {
                    if (unmatched < 10) {
                        std::cerr << "[ri_pose_from_coords] No match for atom '" << atom_data.atom_name
                                  << "' in res " << rosetta_resnum << " (" << res_data.res_name << ")" << std::endl;
                    }
                    unmatched++;
                }
            }
        }
        if (unmatched > 0) {
            std::cerr << "[ri_pose_from_coords] Total unmatched atoms: " << unmatched << std::endl;
        }

        // Batch set coordinates - this properly updates internal coordinates
        wrapper->pose->batch_set_xyz(atom_ids, coords);

        // Track which atoms were set (for export)
        for (const auto& aid : atom_ids) {
            wrapper->set_atoms.insert(aid);
        }

        // Debug: log how many atoms were set
        std::cerr << "[ri_pose_from_coords] Set " << atom_ids.size() << " of "
                  << num_atoms << " input atoms on pose with "
                  << wrapper->pose->total_residue() << " residues" << std::endl;

        return wrapper;
    } catch (...) {
        return nullptr;
    }
}

extern "C" RIPose* ri_pose_from_sequence(const char* sequence) {
    if (!g_initialized) return nullptr;
    if (!sequence) return nullptr;

    try {
        auto* wrapper = new RIPose();
        wrapper->pose = utility::pointer::make_shared<core::pose::Pose>();
        core::pose::make_pose_from_sequence(*wrapper->pose, std::string(sequence),
            *core::chemical::ChemicalManager::get_instance()->residue_type_set(core::chemical::FA_STANDARD));
        return wrapper;
    } catch (...) {
        return nullptr;
    }
}

extern "C" void ri_pose_free(RIPose* pose) {
    delete pose;
}

extern "C" RIPose* ri_pose_clone(const RIPose* pose) {
    if (!pose || !pose->pose) return nullptr;

    try {
        auto* wrapper = new RIPose();
        wrapper->pose = pose->pose->clone();
        wrapper->set_atoms = pose->set_atoms;  // Copy tracked atoms
        return wrapper;
    } catch (...) {
        return nullptr;
    }
}

/* ============================================================================
 * Pose Queries
 * ============================================================================ */

extern "C" size_t ri_pose_num_residues(const RIPose* pose) {
    if (!pose || !pose->pose) return 0;
    return pose->pose->total_residue();
}

extern "C" char* ri_pose_sequence(const RIPose* pose) {
    if (!pose || !pose->pose) return nullptr;

    try {
        std::string seq = pose->pose->sequence();
        char* result = (char*)malloc(seq.length() + 1);
        if (result) {
            std::memcpy(result, seq.c_str(), seq.length() + 1);
        }
        return result;
    } catch (...) {
        return nullptr;
    }
}

// Helper to write big-endian values to buffer
static void write_u32_be(uint8_t* data, uint32_t val) {
    data[0] = (val >> 24) & 0xFF;
    data[1] = (val >> 16) & 0xFF;
    data[2] = (val >> 8) & 0xFF;
    data[3] = val & 0xFF;
}

static void write_i32_be(uint8_t* data, int32_t val) {
    write_u32_be(data, static_cast<uint32_t>(val));
}

static void write_f32_be(uint8_t* data, float val) {
    uint32_t bits;
    std::memcpy(&bits, &val, sizeof(float));
    write_u32_be(data, bits);
}

extern "C" uint8_t* ri_pose_to_coords(const RIPose* pose, size_t* out_len) {
    if (!pose || !pose->pose || !out_len) return nullptr;

    try {
        // Only export atoms that were in the original input (tracked in set_atoms)
        uint32_t total_atoms = pose->set_atoms.size();

        // Allocate buffer: 8 (magic) + 4 (count) + 24 * num_atoms
        size_t buf_size = 12 + total_atoms * 24;
        uint8_t* buffer = (uint8_t*)malloc(buf_size);
        if (!buffer) return nullptr;

        // Write header
        std::memcpy(buffer, "COORDS00", 8);
        write_u32_be(buffer + 8, total_atoms);

        // Write atom data (only atoms that were set)
        uint8_t* ptr = buffer + 12;
        for (core::Size res_i = 1; res_i <= pose->pose->total_residue(); ++res_i) {
            const core::conformation::Residue& res = pose->pose->residue(res_i);
            std::string res_name = res.name3();
            uint8_t chain_id = 'A'; // Default chain

            for (core::Size atom_i = 1; atom_i <= res.natoms(); ++atom_i) {
                core::id::AtomID aid(atom_i, res_i);
                // Only export if this atom was in the original input
                if (pose->set_atoms.find(aid) == pose->set_atoms.end()) {
                    continue;
                }

                const core::Vector& xyz = res.xyz(atom_i);
                std::string atom_name = res.atom_name(atom_i);

                // Trim whitespace first, then pad to 4 chars (right-padded)
                while (!atom_name.empty() && atom_name.front() == ' ') atom_name.erase(0, 1);
                while (!atom_name.empty() && atom_name.back() == ' ') atom_name.pop_back();
                while (atom_name.length() < 4) atom_name += ' ';
                atom_name = atom_name.substr(0, 4);

                // Pad/trim res name to 3 chars
                while (res_name.length() < 3) res_name += ' ';

                write_f32_be(ptr, static_cast<float>(xyz.x())); ptr += 4;
                write_f32_be(ptr, static_cast<float>(xyz.y())); ptr += 4;
                write_f32_be(ptr, static_cast<float>(xyz.z())); ptr += 4;
                *ptr++ = chain_id;
                std::memcpy(ptr, res_name.c_str(), 3); ptr += 3;
                write_i32_be(ptr, static_cast<int32_t>(res_i)); ptr += 4;
                std::memcpy(ptr, atom_name.c_str(), 4); ptr += 4;
            }
        }

        std::cerr << "[ri_pose_to_coords] Exported " << total_atoms << " atoms (only input atoms) from "
                  << pose->pose->total_residue() << " residues" << std::endl;

        *out_len = buf_size;
        return buffer;
    } catch (...) {
        *out_len = 0;
        return nullptr;
    }
}

/* ============================================================================
 * Scoring
 * ============================================================================ */

extern "C" RIScoreFunction* ri_scorefunction_ref2015(void) {
    if (!g_initialized) return nullptr;

    try {
        auto* wrapper = new RIScoreFunction();
        wrapper->sfxn = core::scoring::get_score_function();
        // Set up for Cartesian minimization (per Foldit's PoseLoopThreadActionCart)
        wrapper->sfxn->set_weight(core::scoring::cart_bonded, 0.5);
        wrapper->sfxn->set_weight(core::scoring::pro_close, 0.0);  // Must disable for cart min
        return wrapper;
    } catch (...) {
        return nullptr;
    }
}

extern "C" void ri_scorefunction_free(RIScoreFunction* sfxn) {
    delete sfxn;
}

extern "C" double ri_score(RIPose* pose, RIScoreFunction* sfxn) {
    if (!pose || !pose->pose || !sfxn || !sfxn->sfxn) return 0.0;

    try {
        return (*sfxn->sfxn)(*pose->pose);
    } catch (...) {
        return 0.0;
    }
}

// Helper to mark all atoms as "set" after an operation that validates all positions
static void mark_all_atoms_set(RIPose* pose) {
    if (!pose || !pose->pose) return;

    pose->set_atoms.clear();
    for (core::Size res_i = 1; res_i <= pose->pose->total_residue(); ++res_i) {
        const core::conformation::Residue& res = pose->pose->residue(res_i);
        for (core::Size atom_i = 1; atom_i <= res.natoms(); ++atom_i) {
            pose->set_atoms.insert(core::id::AtomID(atom_i, res_i));
        }
    }
}

/* ============================================================================
 * Packing
 * ============================================================================ */

extern "C" int ri_pack_rotamers(RIPose* pose, RIScoreFunction* sfxn) {
    if (!pose || !pose->pose) return RI_ERROR_NULL_POINTER;
    if (!sfxn || !sfxn->sfxn) return RI_ERROR_NULL_POINTER;

    try {
        core::pack::task::PackerTaskOP task =
            core::pack::task::TaskFactory::create_packer_task(*pose->pose);
        task->restrict_to_repacking();

        core::pack::pack_rotamers(*pose->pose, *sfxn->sfxn, task);

        // After packing, all sidechain atoms have valid positions - mark them for export
        // This is important after mutations when new atoms need to be exported
        mark_all_atoms_set(pose);

        return RI_SUCCESS;
    } catch (...) {
        return RI_ERROR_ROSETTA_EXCEPTION;
    }
}

extern "C" int ri_pack_rotamers_subset(
    RIPose* pose,
    RIScoreFunction* sfxn,
    const uint32_t* residue_indices,
    size_t num_residues
) {
    if (!pose || !pose->pose) return RI_ERROR_NULL_POINTER;
    if (!sfxn || !sfxn->sfxn) return RI_ERROR_NULL_POINTER;
    if (!residue_indices || num_residues == 0) return RI_ERROR_NULL_POINTER;

    try {
        core::pack::task::PackerTaskOP task =
            core::pack::task::TaskFactory::create_packer_task(*pose->pose);

        // Prevent repacking for all residues first
        task->restrict_to_repacking();
        for (size_t i = 1; i <= pose->pose->total_residue(); ++i) {
            task->nonconst_residue_task(i).prevent_repacking();
        }

        // Enable repacking only for specified residues
        for (size_t i = 0; i < num_residues; ++i) {
            uint32_t res_idx = residue_indices[i];
            if (res_idx >= 1 && res_idx <= pose->pose->total_residue()) {
                task->nonconst_residue_task(res_idx).restrict_to_repacking();
            }
        }

        core::pack::pack_rotamers(*pose->pose, *sfxn->sfxn, task);
        return RI_SUCCESS;
    } catch (...) {
        return RI_ERROR_ROSETTA_EXCEPTION;
    }
}

/* ============================================================================
 * Minimization
 * ============================================================================ */

extern "C" int ri_minimize(RIPose* pose, RIScoreFunction* sfxn, uint32_t max_iterations) {
    if (!pose || !pose->pose) return RI_ERROR_NULL_POINTER;
    if (!sfxn || !sfxn->sfxn) return RI_ERROR_NULL_POINTER;

    try {
        core::kinematics::MoveMapOP movemap(new core::kinematics::MoveMap());
        movemap->set_bb(true);
        movemap->set_chi(true);

        double score_before = (*sfxn->sfxn)(*pose->pose);
        uint32_t max_iter = (max_iterations > 0) ? max_iterations : 50;

        // Try Cartesian minimization first
        {
            core::optimization::CartesianMinimizer minimizer;
            core::optimization::MinimizerOptions options("lbfgs_armijo_nonmonotone_atol", 0.01, true);
            options.max_iter(max_iter);
            minimizer.run(*pose->pose, *movemap, *sfxn->sfxn, options);
        }

        double score_after = (*sfxn->sfxn)(*pose->pose);

        // If Cartesian minimization didn't help much (score still very high or unchanged),
        // try torsion-space minimization as fallback
        if (score_after > 10000 && (score_after >= score_before * 0.99)) {
            std::cerr << "[ri_minimize] Cartesian minimization ineffective (score: " << score_after
                      << "), trying torsion-space..." << std::endl;

            core::optimization::AtomTreeMinimizer at_minimizer;
            core::optimization::MinimizerOptions at_options("lbfgs_armijo_nonmonotone", 0.01, true);
            at_options.max_iter(max_iter);
            at_minimizer.run(*pose->pose, *movemap, *sfxn->sfxn, at_options);

            double score_torsion = (*sfxn->sfxn)(*pose->pose);
            std::cerr << "[ri_minimize] After torsion-space: " << score_torsion << std::endl;
        }

        // After minimization, all atoms have valid positions - mark them for export
        mark_all_atoms_set(pose);

        return RI_SUCCESS;
    } catch (...) {
        return RI_ERROR_ROSETTA_EXCEPTION;
    }
}

extern "C" int ri_minimize_backbone(RIPose* pose, RIScoreFunction* sfxn, uint32_t max_iterations) {
    if (!pose || !pose->pose) return RI_ERROR_NULL_POINTER;
    if (!sfxn || !sfxn->sfxn) return RI_ERROR_NULL_POINTER;

    try {
        core::kinematics::MoveMapOP movemap(new core::kinematics::MoveMap());
        movemap->set_bb(true);
        movemap->set_chi(false);

        core::optimization::CartesianMinimizer minimizer;
        core::optimization::MinimizerOptions options("lbfgs_armijo_nonmonotone_atol", 0.01, true);
        options.max_iter(max_iterations > 0 ? max_iterations : 50);

        minimizer.run(*pose->pose, *movemap, *sfxn->sfxn, options);
        return RI_SUCCESS;
    } catch (...) {
        return RI_ERROR_ROSETTA_EXCEPTION;
    }
}

extern "C" int ri_minimize_sidechains(RIPose* pose, RIScoreFunction* sfxn, uint32_t max_iterations) {
    if (!pose || !pose->pose) return RI_ERROR_NULL_POINTER;
    if (!sfxn || !sfxn->sfxn) return RI_ERROR_NULL_POINTER;

    try {
        core::kinematics::MoveMapOP movemap(new core::kinematics::MoveMap());
        movemap->set_bb(false);
        movemap->set_chi(true);

        core::optimization::CartesianMinimizer minimizer;
        core::optimization::MinimizerOptions options("lbfgs_armijo_nonmonotone_atol", 0.01, true);
        options.max_iter(max_iterations > 0 ? max_iterations : 50);

        minimizer.run(*pose->pose, *movemap, *sfxn->sfxn, options);
        return RI_SUCCESS;
    } catch (...) {
        return RI_ERROR_ROSETTA_EXCEPTION;
    }
}

/* ============================================================================
 * Mutation
 * ============================================================================ */

extern "C" int ri_mutate_residue(RIPose* pose, uint32_t residue_index, char new_aa) {
    if (!pose || !pose->pose) return RI_ERROR_NULL_POINTER;
    if (residue_index < 1 || residue_index > pose->pose->total_residue()) {
        return RI_ERROR_NULL_POINTER;
    }

    try {
        // Get the 3-letter name for the new amino acid
        core::chemical::AA aa_enum = core::chemical::aa_from_oneletter_code(new_aa);
        std::string res_name = core::chemical::name_from_aa(aa_enum);

        // Get the new residue type
        core::chemical::ResidueTypeCOP new_restype = core::pose::get_restype_for_pose(
            *pose->pose, res_name, pose->pose->residue_type(residue_index).mode());

        // Create the new residue
        core::conformation::ResidueOP new_res = core::conformation::ResidueFactory::create_residue(
            *new_restype, pose->pose->residue(residue_index), pose->pose->conformation());

        // Copy coordinates from old residue where possible
        core::conformation::copy_residue_coordinates_and_rebuild_missing_atoms(
            pose->pose->residue(residue_index), *new_res, pose->pose->conformation(), true);

        // Replace the residue
        pose->pose->replace_residue(residue_index, *new_res, false);

        return RI_SUCCESS;
    } catch (...) {
        return RI_ERROR_ROSETTA_EXCEPTION;
    }
}

/* ============================================================================
 * Idealization
 * ============================================================================ */

extern "C" int ri_idealize(RIPose* pose) {
    if (!pose || !pose->pose) return RI_ERROR_NULL_POINTER;

    try {
        // Idealize each position to fix unrealistic bond lengths/angles
        // This is essential for structures from ML models (like RFD3) that may have
        // non-ideal geometry that causes minimization to fail
        core::Size nres = pose->pose->total_residue();
        std::cerr << "[ri_idealize] Idealizing " << nres << " residues..." << std::endl;

        for (core::Size i = 1; i <= nres; ++i) {
            core::conformation::idealize_position(i, pose->pose->conformation());
        }

        std::cerr << "[ri_idealize] Idealization complete" << std::endl;

        // After idealization, all atoms have valid positions
        mark_all_atoms_set(pose);

        return RI_SUCCESS;
    } catch (const std::exception& e) {
        std::cerr << "[ri_idealize] Exception: " << e.what() << std::endl;
        return RI_ERROR_ROSETTA_EXCEPTION;
    } catch (...) {
        std::cerr << "[ri_idealize] Unknown exception" << std::endl;
        return RI_ERROR_ROSETTA_EXCEPTION;
    }
}

/* ============================================================================
 * Memory Management
 * ============================================================================ */

extern "C" void ri_free(void* ptr) {
    free(ptr);
}
