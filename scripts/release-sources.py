#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Generate redistribution notices or a separate, commit-pinned source release.

Requires Python 3.9 or newer; no third-party Python packages or signing credentials.
Run from any directory:
  python3 scripts/release-sources.py notices --resources dist/LinkLite.app/Contents/Resources
  python3 scripts/release-sources.py source --commit <full-commit-id> --output dist/LinkLite-0.3.7-source.tar.gz

The source command needs Git, Cargo and network access to the locked upstream sources.
It never builds code. Archive metadata is normalized to the app commit timestamp.
"""

import argparse
import gzip
import json
from pathlib import Path, PurePosixPath
import plistlib
import re
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def run(args, cwd=ROOT):
    return subprocess.check_output(args, cwd=cwd, text=True)


def cargo_command():
    if shutil.which("rustup"):
        result = subprocess.run(["rustup", "run", "stable", "cargo", "--version"],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if result.returncode == 0:
            return ["rustup", "run", "stable", "cargo"]
    return ["cargo"]


def metadata(root):
    # Include build-time and other-platform crates too, rather than under-reporting notices.
    return json.loads(run(cargo_command() + ["metadata", "--locked", "--format-version", "1",
                                           "--manifest-path", "engine/Cargo.toml"], cwd=root))


def copy_required(source, destination):
    if not source.is_file() or source.stat().st_size == 0:
        raise RuntimeError("Missing or empty required notice: " + str(source))
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)


def notices(resources):
    resources.mkdir(parents=True, exist_ok=True)
    licenses = resources / "ThirdPartyLicenses"
    if licenses.exists():
        raise RuntimeError("Refusing to reuse an existing notice directory: " + str(licenses))
    copy_required(ROOT / "LICENSE", resources / "LinkLite-LICENSE.txt")
    copy_required(ROOT / ".build/artifacts/sparkle/Sparkle/LICENSE",
                  resources / "Sparkle-LICENSE.txt")
    for name in ("GPL-2.0", "LGPL-2.1"):
        copy_required(ROOT / "vendor/dlcore/LICENSES" / name, licenses / "Vino" / name)
    copy_required(ROOT / "vendor/dlcore/ORIGIN.md", licenses / "Vino/ORIGIN.md")
    copy_required(ROOT / "vendor/libusb1-sys/LICENSE", licenses / "libusb1-sys-0.7.0/LICENSE")
    copy_required(ROOT / "vendor/libusb1-sys/libusb/COPYING", licenses / "libusb/COPYING")
    copy_required(ROOT / "vendor/libusb1-sys/libusb/AUTHORS", licenses / "libusb/AUTHORS")
    lines = [
        "LinkLite redistribution notices",
        "",
        "LinkLite: GPL-2.0-or-later; see LinkLite-LICENSE.txt (GPL version 2 text).",
        "Vino: see ThirdPartyLicenses/Vino for retained GPL-2.0 and LGPL-2.1 texts and provenance.",
        "libusb1-sys bindings: MIT; native statically linked libusb: LGPL-2.1-or-later.",
        "See ThirdPartyLicenses/libusb1-sys-0.7.0 and ThirdPartyLicenses/libusb.",
        "Sparkle: see Sparkle-LICENSE.txt, including its bundled external notices.",
        "Corresponding app/dependency source is supplied separately as the source release asset.",
        "",
        "Locked Rust dependency inventory (includes build-time and other-platform dependencies).",
        "License expressions below are the upstream package metadata, not a legal determination.",
        "All discovered LICENSE, COPYING, NOTICE and COPYRIGHT files are retained verbatim.",
        "",
    ]
    local = {
        "dllite-engine": ROOT / "engine/Cargo.toml",
        "kernel-shim": ROOT / "vendor/dlcore/kernel-shim/Cargo.toml",
        "dlcore": ROOT / "vendor/dlcore/dlcore/Cargo.toml",
        "dlcore-usb": ROOT / "vendor/dlcore/dlcore-usb/Cargo.toml",
        "libusb1-sys": ROOT / "vendor/libusb1-sys/Cargo.toml",
    }
    for package in sorted(metadata(ROOT)["packages"], key=lambda p: (p["name"], p["version"])):
        name, version = package["name"], package["version"]
        license_expression = package["license"]
        base = Path(package["manifest_path"]).parent
        lines.append(f"{name} {version}: {license_expression or 'see license file'}")
        if package["source"] is None:
            if name not in local or Path(package["manifest_path"]).resolve() != local[name].resolve():
                raise RuntimeError("Unreviewed local dependency: " + name)
            continue
        # These are the license layouts of the pinned registry graph. A changed license
        # or a missing half of a dual-license grant must not silently lose its notice.
        expected = {
            "MIT": [("LICENSE", "LICENSE-MIT")],
            "BSD-3-Clause": [("LICENSE",)],
            "MIT OR Apache-2.0": [("LICENSE-MIT",), ("LICENSE-APACHE",)],
            "Apache-2.0 OR MIT": [("LICENSE-MIT",), ("LICENSE-APACHE",)],
            "MIT/Apache-2.0": [("LICENSE-MIT",), ("LICENSE-APACHE",)],
            "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT": [
                ("LICENSE-MIT",), ("LICENSE-APACHE",), ("LICENSE-Apache-2.0_WITH_LLVM-exception",)],
        }
        if license_expression not in expected:
            raise RuntimeError("Unreviewed dependency license: " + name + " " + str(license_expression))
        for alternatives in expected[license_expression]:
            if not any((base / filename).is_file() and (base / filename).stat().st_size
                       for filename in alternatives):
                raise RuntimeError("Missing required license for " + name + ": " + " or ".join(alternatives))
        files = set()
        for path in base.rglob("*"):
            if path.is_file() and path.name.lower().startswith(("license", "copying", "notice", "copyright")):
                files.add(path)
        if package.get("license_file"):
            declared = (base / package["license_file"]).resolve()
            declared.relative_to(base.resolve())
            files.add(declared)
        if not files or not any(p.name.lower().startswith(("license", "copying")) for p in files):
            raise RuntimeError("No license text found for " + name + " " + version)
        for path in sorted(files):
            relative = path.relative_to(base)
            destination = licenses / (name + "-" + version) / relative
            copy_required(path, destination)
            lines.append("  " + destination.relative_to(resources).as_posix())
    (resources / "ThirdPartyNotices.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")


def export_commit(repository, commit, destination, scratch, app=False):
    # git archive reads only the committed tree, never local keys, caches or user files.
    tree = run(["git", "ls-tree", "-r", commit], cwd=repository)
    if any(line.startswith("160000 ") for line in tree.splitlines()):
        raise RuntimeError("Submodules require explicit source packaging; refusing incomplete export")
    archive = scratch / "tree.tar"
    subprocess.run(["git", "archive", "--format=tar", "--output", str(archive), commit],
                   cwd=repository, check=True)
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive) as source:
        for member in source.getmembers():
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts or ".git" in path.parts:
                raise RuntimeError("Unsafe archive path: " + member.name)
            if app:
                forbidden = {".build", "target", "dist", ".git", ".DS_Store", ".ssh",
                             ".gnupg", "credentials", "credentials.toml", ".npmrc", ".netrc"}
                if (forbidden.intersection(path.parts)
                        or any(part.startswith(".env") for part in path.parts)
                        or path.suffix.lower() in {".p12", ".pfx", ".p8", ".key", ".pem", ".keychain", ".keychain-db"}):
                    raise RuntimeError("Refusing app source containing credentials/build output: " + member.name)
            if not (member.isfile() or member.isdir() or member.issym()):
                raise RuntimeError("Unsupported archive entry: " + member.name)
            if member.issym():
                target = (destination / member.name).parent / member.linkname
                target.resolve().relative_to(destination.resolve())
        # Entries have been validated above, including symlink containment.
        source.extractall(destination)
    archive.unlink()


def source_release(commit, output):
    if not re.fullmatch(r"[0-9a-fA-F]{40}", commit):
        raise RuntimeError("--commit must be a full 40-character Git commit ID")
    commit = run(["git", "rev-parse", "--verify", commit + "^{commit}"]).strip()
    timestamp = int(run(["git", "show", "-s", "--format=%ct", commit]).strip())
    output = output.resolve()
    if output.exists():
        raise RuntimeError("Refusing to overwrite " + str(output))
    with tempfile.TemporaryDirectory(prefix="linklite-source-") as temporary:
        scratch = Path(temporary)
        root = scratch / "app"
        export_commit(ROOT, commit, root, scratch, app=True)
        for required in ("CONTRIBUTING.md", "LICENSE", "engine/Cargo.lock", "Package.resolved",
                         "scripts/build-app.sh", "scripts/release-sources.py"):
            if not (root / required).is_file():
                raise RuntimeError("Commit is missing required source-release input: " + required)
        with (root / "Resources/Info.plist").open("rb") as file:
            version = plistlib.load(file)["CFBundleShortVersionString"]
        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
            raise RuntimeError("Unexpected app version: " + version)
        archive_root = "LinkLite-" + version + "-source"
        # Do not silently replace a committed Cargo configuration or source directory.
        config = root / ".cargo/config.toml"
        crates = root / "vendor/crates"
        if config.exists() or (root / ".cargo/config").exists() or crates.exists():
            raise RuntimeError("Source tree already contains a Cargo vendor configuration")
        cargo_config = run(cargo_command() + ["vendor", "--locked", "--versioned-dirs",
                                             "--manifest-path", "engine/Cargo.toml", "vendor/crates"], cwd=root)
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(cargo_config, encoding="utf-8")
        pins = json.loads((root / "Package.resolved").read_text())["pins"]
        if len(pins) != 1 or pins[0]["identity"] != "sparkle":
            raise RuntimeError("Swift dependency set changed; source packaging needs review")
        sparkle = pins[0]
        revision = sparkle["state"]["revision"]
        url = sparkle["location"]
        if url != "https://github.com/sparkle-project/Sparkle" or not re.fullmatch(r"[0-9a-f]{40}", revision):
            raise RuntimeError("Unexpected Sparkle source pin")
        repository = scratch / "sparkle.git"
        subprocess.run(["git", "init", "--bare", str(repository)], check=True)
        subprocess.run(["git", "-C", str(repository), "fetch", "--depth=1", url, revision], check=True)
        sparkle_source = root / "vendor/sparkle-source"
        if sparkle_source.exists():
            raise RuntimeError("Source tree already contains vendor/sparkle-source")
        export_commit(repository, revision, sparkle_source, scratch)
        for required in ("LICENSE", "Package.swift", "Sparkle.xcodeproj/project.pbxproj"):
            if not (sparkle_source / required).is_file():
                raise RuntimeError("Missing Sparkle source input: " + required)
        instructions = f"""LinkLite {version} corresponding source release

App Git commit: {commit}
Sparkle Git source: {url}
Sparkle revision: {revision}
Sparkle version: {sparkle['state']['version']}

The app tree is exported from the specified commit, not from the working directory.
Added release material: this file, .cargo/config.toml, vendor/crates, and
vendor/sparkle-source. engine/Cargo.lock retains the exact crate versions and
registry checksums. Cargo vendor retains per-file checksums and upstream notices.
Vino and the patched libusb1-sys/native libusb sources are in the committed vendor tree.
Sparkle's full pinned Git source, including its bundled third-party sources and
Xcode build project, is in vendor/sparkle-source. No signing keys, Git databases,
local build outputs or untracked files are copied.

Building / modifying
--------------------
Read CONTRIBUTING.md for Apple Silicon, macOS SDK/Xcode and Rust requirements.
Python 3.9+ is also needed for the build-time notice collector (standard library only).
From this extracted directory:
  MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --locked --offline --release --manifest-path engine/Cargo.toml
  scripts/build-app.sh

The Cargo source configuration uses vendor/crates; the Rust dependency sources
needed by the locked app graph are self-contained, including build dependencies
and lockfile-only platform dependencies. The app, Vino and native libusb can be
modified and rebuilt/relinked from these sources. Keep the source tree together.

This is not an offline SDK/toolchain distribution or a bit-for-bit binary recipe.
Apple's SDK, system frameworks, Xcode/Swift and Rust compiler/standard library
must be installed separately. Toolchain versions are not pinned by this archive.
The normal SwiftPM build still uses Package.resolved and downloads Sparkle's
pinned binary artifact from upstream; network access is needed unless cached.
The included Sparkle source/Xcode project is available for rebuilding or modifying
that framework, but the default app build does not substitute it for the SwiftPM
binary target. No private signing identity is needed for an ad-hoc local build.

License texts and per-file notices remain authoritative. This source bundle and
notice collection are redistribution engineering, not legal certification.
"""
        (root / "SOURCE-RELEASE.txt").write_text(instructions, encoding="utf-8")
        output.parent.mkdir(parents=True, exist_ok=True)
        # Sorted names, fixed times/ownership and gzip headers avoid temporary-path/time noise.
        with output.open("xb") as raw:
            try:
                with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
                    with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                        for path in [root] + sorted(root.rglob("*")):
                            name = archive_root if path == root else archive_root + "/" + path.relative_to(root).as_posix()
                            info = archive.gettarinfo(str(path), arcname=name)
                            info.uid = info.gid = 0
                            info.uname = info.gname = ""
                            info.mtime = timestamp
                            info.pax_headers = {}
                            info.mode = 0o755 if path.is_dir() or info.mode & 0o111 else 0o644
                            if info.isfile():
                                with path.open("rb") as data:
                                    archive.addfile(info, data)
                            else:
                                archive.addfile(info)
            except BaseException:
                output.unlink(missing_ok=True)
                raise
    print("Created " + str(output) + " from " + commit)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    notice_parser = commands.add_parser("notices", help="collect required notices before app signing")
    notice_parser.add_argument("--resources", type=Path, required=True)
    source_parser = commands.add_parser("source", help="archive committed app and locked dependency sources")
    source_parser.add_argument("--commit", required=True)
    source_parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "notices":
        notices(args.resources.resolve())
    else:
        source_release(args.commit, args.output)


if __name__ == "__main__":
    main()
