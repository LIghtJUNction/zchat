#!/usr/bin/env python3
"""Build native distribution packages with the checked-out zdistro source."""
from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import struct
import subprocess
import sys
import tempfile
import tomllib

from release import ReleaseError, ROOT, digest, render_toml, repository, run, versions

HOST = {"Linux": "linux", "Darwin": "macos", "Windows": "windows"}
DEFAULT_FORMATS = {"linux": ["tar-gz", "zip", "deb"], "macos": ["zip", "tar-gz", "pkg", "dmg"], "windows": ["zip", "msi"]}
ALLOWED_FORMATS = {"linux": {"tar-gz", "zip", "deb", "arch"}, "macos": {"zip", "tar-gz", "pkg", "dmg"}, "windows": {"zip", "msi"}}


def native_identity(path: Path) -> tuple[str, str]:
    with path.open("rb") as stream:
        header = stream.read(64)
        if header.startswith(b"\x7fELF") and len(header) >= 20:
            byteorder = "<" if header[5] == 1 else ">" if header[5] == 2 else None
            if byteorder is None or header[4] != 2:
                raise ReleaseError(f"Expected a 64-bit native payload: {path.name}")
            machine = struct.unpack_from(byteorder + "H", header, 18)[0]
            architecture = {62: "x86_64", 183: "aarch64"}.get(machine)
            system = "linux"
        elif header.startswith(b"MZ") and len(header) >= 64:
            offset = struct.unpack_from("<I", header, 60)[0]
            if offset > 16 * 1024 * 1024:
                raise ReleaseError("Invalid PE header offset")
            stream.seek(offset)
            pe = stream.read(6)
            if len(pe) != 6 or pe[:4] != b"PE\0\0":
                raise ReleaseError(f"Missing PE header: {path.name}")
            architecture = {0x8664: "x86_64", 0xAA64: "aarch64"}.get(struct.unpack_from("<H", pe, 4)[0])
            system = "windows"
        elif header[:4] in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf") and len(header) >= 8:
            byteorder = "<" if header[:4] == b"\xcf\xfa\xed\xfe" else ">"
            architecture = {0x01000007: "x86_64", 0x0100000C: "aarch64"}.get(struct.unpack_from(byteorder + "I", header, 4)[0])
            system = "macos"
        else:
            raise ReleaseError(f"Unsupported native executable header: {path.name}")
    if architecture is None:
        raise ReleaseError(f"Unsupported native machine architecture: {path.name}")
    return system, architecture


def file_rule(source: str, destination: str, executable: bool = False) -> dict:
    return {"source": source, "destination": destination, "mode": "0755" if executable else "0644"}


def make_manifest(template: dict, system: str, architecture: str, epoch: int) -> dict:
    if system not in ALLOWED_FORMATS or architecture not in ("x86_64", "aarch64"):
        raise ReleaseError("Supported native hosts are Linux, macOS and Windows on x86_64/aarch64")
    document = copy.deepcopy(template)
    document["source_date_epoch"] = epoch
    document["build"] = {"kind": "none"}
    document["targets"] = {name: value for name, value in document["targets"].items() if name in ALLOWED_FORMATS[system]}
    license_rule = file_rule("payload/LICENSE", "LICENSE")
    readme_rule = file_rule("payload/README.md", "README.md")
    if system == "linux":
        portable = [file_rule("payload/zchat", "zchat", True), file_rule("payload/libzchat_providers.so", "libzchat_providers.so", True), license_rule, readme_rule]
        native = [file_rule("payload/zchat", "usr/bin/zchat", True), file_rule("payload/libzchat_providers.so", "usr/lib/zchat/libzchat_providers.so", True), file_rule("payload/zchat.desktop", "usr/share/applications/zchat.desktop"), file_rule("payload/zchat.svg", "usr/share/icons/hicolor/scalable/apps/zchat.svg"), file_rule("payload/LICENSE", "usr/share/licenses/zchat/LICENSE"), file_rule("payload/README.md", "usr/share/doc/zchat/README.md")]
        document["files"] = portable
        document["targets"]["deb"].update({"architecture": "amd64" if architecture == "x86_64" else "arm64", "files": native})
        document["targets"]["arch"].update({"architecture": architecture, "files": native})
    elif system == "windows":
        document["files"] = [file_rule("payload/zchat.exe", "zchat.exe", True), file_rule("payload/zchat_providers.dll", "zchat_providers.dll", True), license_rule, readme_rule]
        document["targets"]["msi"]["architecture"] = "x64" if architecture == "x86_64" else "arm64"
    else:
        document["files"] = [file_rule("payload/zchat.app", "zchat.app"), license_rule, readme_rule]
        # Directory rules preserve file permissions when no mode is specified.
        document["files"][0].pop("mode")
        document["targets"]["pkg"]["files"] = [{"source": "payload/zchat.app", "destination": "Applications/zchat.app"}, file_rule("payload/zchat-launcher", "usr/local/bin/zchat", True), file_rule("payload/LICENSE", "usr/local/share/licenses/zchat/LICENSE")]
    return document


def invoke_zdistro(root: Path, *arguments: str, cwd: Path) -> str:
    env = dict(os.environ)
    env["PYTHONPATH"] = str(root) + (os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
    result = subprocess.run([sys.executable, "-m", "zdistro", *arguments], cwd=cwd, env=env, text=True, capture_output=True)
    if result.returncode:
        raise ReleaseError(f"zdistro {arguments[0]} failed ({result.returncode}):\n{result.stdout[-6000:]}\n{result.stderr[-6000:]}")
    return result.stdout


def package(binary: Path, plugin: Path, zdistro: Path, output: Path, formats: list[str], root: Path = ROOT) -> dict:
    binary, plugin, zdistro, output = (value.resolve() for value in (binary, plugin, zdistro, output))
    if not (zdistro / "zdistro/__main__.py").is_file():
        raise ReleaseError("--zdistro-root must point to a zdistro source checkout")
    if repository(run("git", "remote", "get-url", "origin", cwd=zdistro).strip()) != "https://github.com/lightjunction/zdistro":
        raise ReleaseError("zdistro source must identify the Project Z repository")
    if run("git", "status", "--porcelain", "--untracked-files=no", cwd=zdistro).strip():
        raise ReleaseError("Use an unmodified zdistro source snapshot")
    zdistro_commit = run("git", "rev-parse", "HEAD", cwd=zdistro).strip()
    system, architecture = native_identity(binary)
    if native_identity(plugin) != (system, architecture):
        raise ReleaseError("Plugin and application target architectures differ")
    if HOST.get(platform.system()) != system:
        raise ReleaseError("Native packaging must run on the executable's host platform")
    version, _ = versions(root)
    actual_version = run(str(binary), "--version", cwd=root).strip()
    if version not in actual_version.split():
        raise ReleaseError(f"Built executable version differs from source {version}: {actual_version}")
    selected = formats or DEFAULT_FORMATS[system]
    if not selected or len(selected) != len(set(selected)) or not set(selected) <= ALLOWED_FORMATS[system]:
        raise ReleaseError(f"Unsupported or duplicate formats for {system}: {selected}")
    commit = run("git", "rev-parse", "HEAD", cwd=root).strip()
    epoch = int(run("git", "show", "-s", "--format=%ct", commit, cwd=root).strip())
    template = tomllib.loads((root / "zdistro.toml").read_text())
    if template["package"]["version"] != version:
        raise ReleaseError("zdistro and Cargo package versions differ")
    output.mkdir(parents=True, exist_ok=True)
    label = f"zchat-{version}-{system}-{architecture}"
    provenance = {"schema": "zchat.native-package.v1", "package": "zchat", "version": version, "source_commit": commit, "zdistro_commit": zdistro_commit, "platform": system, "architecture": architecture, "binary_sha256": digest(binary.read_bytes()), "plugin_sha256": digest(plugin.read_bytes()), "runtime_check": actual_version, "signed": False, "installed": False}
    with tempfile.TemporaryDirectory(prefix="zchat-package-", dir=output.parent) as temporary:
        work = Path(temporary)
        payload = work / "payload"
        payload.mkdir()
        for name in ("LICENSE", "README.md"):
            shutil.copyfile(root / name, payload / name)
        if system == "macos":
            app = payload / "zchat.app/Contents"
            executable_dir = app / "MacOS"
            executable_dir.mkdir(parents=True)
            shutil.copy2(binary, executable_dir / "zchat")
            shutil.copy2(plugin, executable_dir / "libzchat_providers.dylib")
            (executable_dir / "zchat").chmod(0o755)
            (executable_dir / "libzchat_providers.dylib").chmod(0o755)
            metadata = {"CFBundleDevelopmentRegion": "en", "CFBundleDisplayName": "zchat", "CFBundleExecutable": "zchat", "CFBundleIdentifier": "dev.projectz.zchat", "CFBundleInfoDictionaryVersion": "6.0", "CFBundleName": "zchat", "CFBundlePackageType": "APPL", "CFBundleShortVersionString": version, "CFBundleVersion": version, "LSMinimumSystemVersion": "13.0", "NSHighResolutionCapable": True}
            (app / "Info.plist").write_bytes(plistlib.dumps(metadata))
            (payload / "zchat-launcher").write_text('#!/bin/sh\nexec /Applications/zchat.app/Contents/MacOS/zchat "$@"\n')
        else:
            executable_name = "zchat.exe" if system == "windows" else "zchat"
            plugin_name = "zchat_providers.dll" if system == "windows" else "libzchat_providers.so"
            shutil.copy2(binary, payload / executable_name)
            shutil.copy2(plugin, payload / plugin_name)
            if system == "linux":
                shutil.copyfile(root / "packaging/zchat.desktop", payload / "zchat.desktop")
                shutil.copyfile(root / "packaging/zchat.svg", payload / "zchat.svg")
        document = make_manifest(template, system, architecture, epoch)
        manifest = work / "zdistro.toml"
        manifest.write_text(render_toml(document))
        plan = json.loads(invoke_zdistro(zdistro, "plan", "--manifest", str(manifest), "--target", ",".join(selected), cwd=work))
        (output / (label + ".plan.json")).write_text(json.dumps(plan, indent=2) + "\n")
        built = json.loads(invoke_zdistro(zdistro, "build", "--manifest", str(manifest), "--target", ",".join(selected), "--out", str(work / "packages"), cwd=work))
        if built.get("skipped") or {item["target"] for item in built.get("built", [])} != set(selected):
            raise ReleaseError("zdistro did not produce every requested native format")
        saved_manifest = output / (label + ".zdistro.toml")
        shutil.copyfile(manifest, saved_manifest)
        artifacts = []
        for item in built["built"]:
            source = Path(item["artifact"]).resolve()
            receipt_path = Path(item["receipt"]).resolve()
            if not source.is_relative_to(work / "packages") or not receipt_path.is_relative_to(work / "packages"):
                raise ReleaseError("zdistro returned an artifact outside its output directory")
            receipt = json.loads(receipt_path.read_text())
            if receipt.get("status") != "verified" or receipt.get("sha256") != digest(source.read_bytes()) or receipt.get("bytes") != source.stat().st_size:
                raise ReleaseError("zdistro artifact and verification receipt disagree")
            suffix = {"tar-gz": ".tar.gz", "arch": ".pkg.tar.gz"}.get(item["target"], "." + item["target"])
            destination = output / (label + suffix)
            if destination.exists() and digest(destination.read_bytes()) != receipt["sha256"]:
                raise ReleaseError(f"Output already contains a different artifact: {destination.name}")
            shutil.copyfile(source, destination)
            receipt["artifact"] = destination.name
            final_receipt = output / (destination.name + ".receipt.json")
            final_receipt.write_text(json.dumps(receipt, indent=2) + "\n")
            verified = json.loads(invoke_zdistro(zdistro, "verify", str(final_receipt), "--manifest", str(saved_manifest), cwd=output))
            artifacts.append({"format": item["target"], "file": destination.name, "receipt": final_receipt.name, "bytes": destination.stat().st_size, "sha256": receipt["sha256"], "verification": verified["verification"]})
    provenance["artifacts"] = artifacts
    (output / (label + ".provenance.json")).write_text(json.dumps(provenance, indent=2) + "\n")
    return provenance


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--plugin", type=Path, required=True)
    parser.add_argument("--zdistro-root", type=Path, required=True)
    parser.add_argument("--out", type=Path, default=ROOT / "dist/release")
    parser.add_argument("--formats", default="", help="Comma-separated native formats; defaults depend on the host")
    args = parser.parse_args()
    try:
        result = package(args.binary, args.plugin, args.zdistro_root, args.out, args.formats.split(",") if args.formats else [])
        print(json.dumps(result, indent=2))
        return 0
    except (ReleaseError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"Packaging failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
