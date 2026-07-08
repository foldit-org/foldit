#!/usr/bin/env python3
"""Assemble per-plugin Python environments for a distribution bundle.

For each shippable Python plugin this copies its conda env into
``<output>/plugins/<id>/env/`` and materializes the *editable* installs
(which otherwise point back at the dev tree via ``.pth`` files) into the
copied env's ``site-packages``. The result is self-contained: a shipped
Foldit needs no pixi and no conda at runtime. The orchestrator points the
worker at ``plugins/<id>/env`` via ``PYTHONHOME`` and boots the embedded
interpreter from there (see ``src/orchestrator/spawn.rs``).

Layout produced (per python plugin)::

    <output>/plugins/<id>/
        plugin.toml
        assets/weights/        (empty; weights download on first run)
        env/                   (the relocated conda env)

NOTE on relocation: the conda env is copied as-is. On macOS the
load-bearing binaries (libpython, torch) use ``@rpath``/``@loader_path``,
so the copy relocates without prefix rewriting; absolute build-time
leftovers (e.g. console-script shebangs, torch's CI rpath) are dead weight,
not blockers, for the embed-libpython inference path. If a future target
needs true prefix relocation, add a conda-pack / pixi-pack pass here.

Usage::

    python pixi_scripts/bundle_runtime.py --output <bundle-dir>
"""

import argparse
import ast
import os
import shutil
import sys
import tomllib
from pathlib import Path

# crates/foldit-runner
PROJECT_ROOT = Path(__file__).resolve().parent.parent
# Plugins live in the repo-root `plugins/` tree (siblings of `crates/`), each
# its own submodule owning its own pixi env at `<plugin>/.pixi/envs/<id>`.
# PROJECT_ROOT is crates/foldit-runner, so climb two levels.
PLUGINS_SRC = PROJECT_ROOT.parent.parent / "plugins"


def _discover_shippable_plugins(plugins_src: Path) -> list[str]:
    """Python plugin ids shipped in the distribution, discovered from the
    plugins root rather than hardcoded. Native plugins (e.g. rosetta) have no
    pixi env and are bundled separately by xtask; the dummy test fixture lives
    outside the plugins root and is never shipped. Each plugin's pixi env name
    matches its id (manifest.python_env() default)."""
    if not plugins_src.is_dir():
        return []
    ids: list[str] = []
    for child in sorted(plugins_src.iterdir()):
        manifest = child / "plugin.toml"
        if not manifest.is_file():
            continue
        with manifest.open("rb") as fh:
            data = tomllib.load(fh)
        if data.get("kind") != "python":
            continue
        ids.append(data.get("id", child.name))
    return ids


# Plugins shipped in the distribution, discovered from PLUGINS_SRC. dummy is
# test-only (outside the plugins root) and not shipped.
SHIPPABLE_PLUGINS = _discover_shippable_plugins(PLUGINS_SRC)

# Per-file exclusions (development cruft never needed at runtime).
EXCLUDE_FILE_SUFFIXES = (
    ".pyc",
    ".pyo",
    ".lib",
    ".pdb",
    ".a",
    ".h",
    ".hpp",
    ".hxx",
    ".cmake",
)

# Directory names dropped wherever they appear in the env.
EXCLUDE_DIR_NAMES = {"include", "cmake", "pkgconfig", "__pycache__"}

# Top-level site-packages entries (and their .dist-info) dropped by package
# name: dev/debug/notebook tooling + libs unused by inference. Curated for
# the foundry/simplefold inference paths.
EXCLUDE_PACKAGES = {
    "_pytest",
    "pytest",
    "debugpy",
    "ipython",
    "ipykernel",
    "jupyter",
    "jupyter_core",
    "jupyter_client",
    "notebook",
    "nbformat",
    "nbconvert",
    "matplotlib",
    "matplotlib_inline",
    "wheel",
    "pip",
    "autopep8",
    "pycodestyle",
    "pydevd_plugins",
    "seaborn",
    "py3dmol",
    "grpc",
    "grpc_tools",
    "wandb",
    "torchvision",
}


def normalize(name: str) -> str:
    """Normalize a package name for comparison."""
    return name.lower().replace("-", "_")


NORMALIZED_EXCLUDE = {normalize(p) for p in EXCLUDE_PACKAGES}


def is_excluded_sp_entry(name: str) -> bool:
    """True if a top-level site-packages entry belongs to an excluded package."""
    base = name
    for suffix in (".dist-info", ".egg-info", ".libs"):
        if base.endswith(suffix):
            base = base[: -len(suffix)]
            # dist-info dirs are `<name>-<version>`; strip the version.
            base = base.rsplit("-", 1)[0]
            break
    return normalize(base) in NORMALIZED_EXCLUDE


def site_packages(env_dir: Path) -> Path:
    """Resolve ``<env>/lib/python3.X/site-packages`` (X discovered on disk)."""
    libdir = env_dir / "lib"
    if libdir.is_dir():
        for entry in sorted(libdir.glob("python3.*")):
            if entry.is_dir():
                return entry / "site-packages"
    return libdir / "python3.12" / "site-packages"


def dir_size_gb(path: Path) -> float:
    total = 0
    for root, _, files in os.walk(path):
        for f in files:
            fp = Path(root) / f
            try:
                total += fp.stat(follow_symlinks=False).st_size
            except OSError:
                pass
    return total / (1024**3)


# --------------------------------------------------------------------------
# env copy
# --------------------------------------------------------------------------
def copy_env(env_src: Path, env_dst: Path) -> None:
    """Copy a conda env, dropping dev cruft and excluded packages.

    Symlinks are dereferenced (``copy2`` follows them) so the bundle is
    self-contained; this duplicates the few symlinked libs, which is an
    accepted cost (de-dup is deferred).
    """
    sp_src = site_packages(env_src)
    for root, dirs, files in os.walk(env_src, followlinks=False):
        root_p = Path(root)
        at_site_packages = root_p == sp_src

        kept_dirs = []
        for d in dirs:
            if d in EXCLUDE_DIR_NAMES or d.endswith(".egg-info"):
                continue
            if at_site_packages and is_excluded_sp_entry(d):
                continue
            kept_dirs.append(d)
        dirs[:] = kept_dirs

        dst_root = env_dst / root_p.relative_to(env_src)
        dst_root.mkdir(parents=True, exist_ok=True)
        for fname in files:
            if fname.endswith(EXCLUDE_FILE_SUFFIXES):
                continue
            if at_site_packages and is_excluded_sp_entry(fname):
                continue
            src_f = root_p / fname
            try:
                shutil.copy2(src_f, dst_root / fname, follow_symlinks=True)
            except (OSError, shutil.Error) as e:
                print(f"    warn: skipped {src_f}: {e}")


# --------------------------------------------------------------------------
# editable materialization
# --------------------------------------------------------------------------
def materialize_editables(sp: Path) -> None:
    """Replace editable .pth/finder shims with real package copies.

    pixi installs every plugin + dep editable, so the copied env's
    site-packages only holds .pth pointers into the dev tree. Copy the
    referenced sources in and delete the shims, leaving a normal env.
    Non-editable .pth (e.g. distutils-precedence.pth) and already-real
    installs (e.g. molex) are left untouched.
    """
    # 1. setuptools finder editables: __editable___<name>_finder.py holds a
    #    dotted-name -> source-path MAPPING (handles remaps like
    #    esm.scripts -> .../scripts). Process shallow keys first so parent
    #    packages exist before their children.
    for finder in sorted(sp.glob("__editable___*_finder.py")):
        mapping = _parse_finder_mapping(finder)
        for dotted in sorted(mapping, key=lambda k: k.count(".")):
            _materialize_dotted(sp, dotted, Path(mapping[dotted]))
        finder.unlink()

    # 2. path editables: each non-import line is a source root added to
    #    sys.path. Copy its top-level packages/modules in.
    pth_files = sorted(
        list(sp.glob("_editable_impl_*.pth")) + list(sp.glob("__editable__.*.pth"))
    )
    for pth in pth_files:
        for line in pth.read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("import "):
                continue  # finder loader line, handled above
            src_root = Path(line)
            if src_root.is_dir():
                _materialize_source_root(sp, src_root)
        pth.unlink()


def _parse_finder_mapping(finder: Path) -> dict:
    """Extract the literal ``MAPPING`` dict from a finder module.

    setuptools writes it as an *annotated* assignment
    (``MAPPING: dict[str, str] = {...}``), so handle both annotated and
    plain assignments.
    """
    tree = ast.parse(finder.read_text())
    for node in tree.body:
        value = None
        if isinstance(node, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == "MAPPING" for t in node.targets
        ):
            value = node.value
        elif (
            isinstance(node, ast.AnnAssign)
            and isinstance(node.target, ast.Name)
            and node.target.id == "MAPPING"
        ):
            value = node.value
        if value is not None:
            return ast.literal_eval(value)
    return {}


def _materialize_source_root(sp: Path, src_root: Path) -> None:
    """Copy each top-level importable package/module from a source root."""
    for entry in sorted(src_root.iterdir()):
        if entry.is_dir() and (entry / "__init__.py").exists():
            _copy_package(sp / entry.name, entry)
        elif (
            entry.is_file()
            and entry.suffix == ".py"
            and entry.name not in ("setup.py", "conftest.py")
        ):
            dst = sp / entry.name
            if not dst.exists():
                shutil.copy2(entry, dst)


def _materialize_dotted(sp: Path, dotted: str, src: Path) -> None:
    """Materialize a finder MAPPING entry at its dotted path under site-packages."""
    dst = sp.joinpath(*dotted.split("."))
    if src.is_dir():
        _copy_package(dst, src)
    elif src.suffix == ".py" and src.exists():
        dst = dst.with_suffix(".py")
        dst.parent.mkdir(parents=True, exist_ok=True)
        if not dst.exists():
            shutil.copy2(src, dst)


def _copy_package(dst: Path, src: Path) -> None:
    """Copy a package dir into site-packages, merging if dst already exists.

    Skips if dst already holds a real install (don't clobber, e.g. molex).
    """
    ignore = shutil.ignore_patterns("__pycache__", "*.pyc", "*.pyo", "*.egg-info")
    if not dst.exists():
        shutil.copytree(src, dst, ignore=ignore)
        return
    # Merge: only add files that aren't already present.
    for root, dirs, files in os.walk(src):
        dirs[:] = [d for d in dirs if d != "__pycache__" and not d.endswith(".egg-info")]
        rel = Path(root).relative_to(src)
        (dst / rel).mkdir(parents=True, exist_ok=True)
        for f in files:
            if f.endswith((".pyc", ".pyo")):
                continue
            tgt = dst / rel / f
            if not tgt.exists():
                shutil.copy2(Path(root) / f, tgt)


# --------------------------------------------------------------------------
# driver
# --------------------------------------------------------------------------
def assemble(output_dir: Path) -> int:
    plugins_out = output_dir / "plugins"
    print(f"Assembling Python plugins into {plugins_out}")
    print(f"  source plugins: {PLUGINS_SRC}")
    print()

    for plugin_id in SHIPPABLE_PLUGINS:
        env_name = plugin_id  # manifest.python_env() default
        # Each plugin owns its pixi env under its own submodule.
        env_src = PLUGINS_SRC / plugin_id / ".pixi" / "envs" / env_name
        if not env_src.is_dir():
            print(
                f"ERROR: env '{env_name}' not found at {env_src}.\n"
                f"  Run 'cargo xtask setup-envs' to create the dev environments."
            )
            return 1

        plugin_out = plugins_out / plugin_id
        env_dst = plugin_out / "env"
        print(f"[{plugin_id}] copying env -> {env_dst}")
        if env_dst.exists():
            shutil.rmtree(env_dst)
        copy_env(env_src, env_dst)

        print(f"[{plugin_id}] materializing editable installs")
        materialize_editables(site_packages(env_dst))

        manifest = PLUGINS_SRC / plugin_id / "plugin.toml"
        shutil.copy2(manifest, plugin_out / "plugin.toml")

        # Ship declared assets (button icons referenced by plugin.toml, etc.).
        # Skip `weights`: it is large and downloaded on first run, so the
        # bundle ships only the empty target dir created below.
        src_assets = PLUGINS_SRC / plugin_id / "assets"
        if src_assets.is_dir():
            for child in src_assets.iterdir():
                if child.name == "weights":
                    continue
                dst = plugin_out / "assets" / child.name
                if child.is_dir():
                    shutil.copytree(
                        child,
                        dst,
                        dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns("__pycache__", "*.pyc", "*.pyo"),
                    )
                else:
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(child, dst)

        # First-run download target for weights (kept empty in the bundle).
        (plugin_out / "assets" / "weights").mkdir(parents=True, exist_ok=True)

        print(f"[{plugin_id}] done ({dir_size_gb(env_dst):.2f} GB)")
        print()

    print("=" * 60)
    print(f"Python plugins assembled at {plugins_out}")
    print("=" * 60)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output",
        type=Path,
        default=PROJECT_ROOT / "bundle",
        help="Bundle root; plugins are written under <output>/plugins/ "
        "(default: ./bundle)",
    )
    args = parser.parse_args()
    return assemble(args.output.resolve())


if __name__ == "__main__":
    sys.exit(main())
