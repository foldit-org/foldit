const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    // Get ROSETTA_SOURCE from environment or use default
    const home = std.posix.getenv("HOME") orelse "/home/user";
    const rosetta_source = std.posix.getenv("ROSETTA_SOURCE") orelse
        b.fmt("{s}/rosetta/source", .{home});

    const lib = b.addLibrary(.{
        .name = "rosetta_interactive",
        .root_module = b.createModule(.{
            .target = target,
            .optimize = optimize,
        }),
        .linkage = .dynamic,
    });

    lib.addCSourceFile(.{
        .file = b.path("src/rosetta_interactive.cpp"),
        .flags = &.{
            "-std=c++14",
            "-DBOOST_DISABLE_THREADS",
            "-DBOOST_ERROR_CODE_HEADER_ONLY",
            "-DBOOST_SYSTEM_NO_DEPRECATED",
            "-DCXX11",
            "-DNDEBUG",
            "-DPTR_STD",
            "-DUNUSUAL_ALLOCATOR_DECLARATION", // Use <vector> include instead of forward-declaring std::allocator
            "-Wno-deprecated", // Boost uses deprecated std::unary_function
            "-Wno-deprecated-declarations",
        },
    });

    // Include paths
    lib.addIncludePath(b.path("include"));
    lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/src", .{rosetta_source}) });
    lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/external", .{rosetta_source}) });
    lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/external/include", .{rosetta_source}) });
    lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/external/boost_submod", .{rosetta_source}) });
    lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/external/dbio", .{rosetta_source}) });
    lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/external/dbio/sqlite3", .{rosetta_source}) });
    lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/external/libxml2/include", .{rosetta_source}) });

    // Platform-specific includes and defines
    const native_target = target.result;
    if (native_target.os.tag == .macos) {
        lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/src/platform/macos", .{rosetta_source}) });
        lib.root_module.addCMacro("MAC", "1");
    } else if (native_target.os.tag == .linux) {
        lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/src/platform/linux", .{rosetta_source}) });
        lib.root_module.addCMacro("LINUX", "1");
    } else if (native_target.os.tag == .windows) {
        lib.addIncludePath(.{ .cwd_relative = b.fmt("{s}/src/platform/windows", .{rosetta_source}) });
        lib.root_module.addCMacro("WIN32", "1");
    }

    // Library search path
    lib.addLibraryPath(b.path("lib"));

    // Link Rosetta libraries
    lib.linkSystemLibrary("core.6");
    lib.linkSystemLibrary("core.5");
    lib.linkSystemLibrary("core.4");
    lib.linkSystemLibrary("core.3");
    lib.linkSystemLibrary("core.2");
    lib.linkSystemLibrary("core.1");
    lib.linkSystemLibrary("basic");
    lib.linkSystemLibrary("numeric");
    lib.linkSystemLibrary("utility");
    lib.linkSystemLibrary("ObjexxFCL");
    lib.linkSystemLibrary("libxml2");
    lib.linkSystemLibrary("rdkit");
    lib.linkSystemLibrary("cmaes");
    lib.linkSystemLibrary("cppdb");
    lib.linkSystemLibrary("sqlite3");
    lib.linkSystemLibrary("z");

    // Link C++ standard library
    lib.linkLibCpp();

    // Install
    b.installArtifact(lib);

    // Also install the header
    b.installFile("include/rosetta_interactive.h", "include/rosetta_interactive.h");
}
