#!/usr/bin/env python3
"""Version, stage and publish zchat from an immutable, verified source snapshot."""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PACKAGES = (
    ("zchat-core", "crates/zchat-core/Cargo.toml"),
    ("zchat-plugin-api", "crates/zchat-plugin-api/Cargo.toml"),
    ("zchat-providers", "crates/zchat-providers/Cargo.toml"),
    ("zchat", "Cargo.toml"),
)
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
DEPENDENCY_KINDS = ("dependencies", "build-dependencies", "dev-dependencies")
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
USER_AGENT = "zchat-release/0.1 (+https://github.com/LIghtJUNction/zchat)"
GENERATED = {"Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"}


class ReleaseError(RuntimeError):
    pass


def run(*command: str, cwd: Path = ROOT) -> str:
    completed = subprocess.run(command, cwd=cwd, text=True, capture_output=True)
    if completed.returncode:
        raise ReleaseError(f"{command[0]} {command[1] if len(command) > 1 else ''} failed ({completed.returncode}):\n{completed.stderr[-12000:]}")
    return completed.stdout


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def repository(value: str) -> str:
    return value.removeprefix("git+").partition("#")[0].partition("?")[0].rstrip("/").removesuffix(".git").lower()


def stable_version(value: str) -> tuple[int, int, int]:
    match = SEMVER.fullmatch(value)
    if not match:
        raise ReleaseError("Use a stable MAJOR.MINOR.PATCH version without leading zeroes")
    return tuple(map(int, match.groups()))


def next_version(current: str, bump: str, custom: str = "") -> str:
    old = stable_version(current)
    if bump == "current":
        if custom:
            raise ReleaseError("A custom version is only accepted with --bump custom")
        return current
    if bump == "custom":
        if stable_version(custom) <= old:
            raise ReleaseError("A custom version must be newer than the current version")
        return custom
    if custom or bump not in ("major", "minor", "patch"):
        raise ReleaseError("Choose current, major, minor, patch or custom")
    index = ("major", "minor", "patch").index(bump)
    numbers = list(old)
    numbers[index:] = [numbers[index] + 1] + [0] * (2 - index)
    return ".".join(map(str, numbers))


def dependency_tables(document: dict):
    for name in DEPENDENCY_KINDS:
        yield document.get(name, {})
    for target in document.get("target", {}).values():
        for name in DEPENDENCY_KINDS:
            yield target.get(name, {})
    yield document.get("workspace", {}).get("dependencies", {})


def toml_value(value) -> str:
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False).replace("\x7f", "\\u007f")
    if isinstance(value, bool):
        return str(value).lower()
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{toml_value(key)} = {toml_value(item)}" for key, item in value.items()) + " }"
    raise ReleaseError(f"Unsupported TOML value {type(value).__name__}")


def render_toml(document: dict) -> str:
    lines = []

    def visit(path: tuple[str, ...], table: dict):
        if path:
            lines.extend(("", "[" + ".".join(map(toml_value, path)) + "]"))
        for name, value in table.items():
            if not isinstance(value, dict):
                lines.append(f"{toml_value(name)} = {toml_value(value)}")
        for name, value in table.items():
            if isinstance(value, dict):
                visit((*path, name), value)

    visit((), document)
    result = "\n".join(lines).lstrip() + "\n"
    if tomllib.loads(result) != document:
        raise ReleaseError("TOML serialization changed the manifest")
    return result


def versions(root: Path = ROOT) -> tuple[str, dict[str, dict]]:
    documents = {path: tomllib.loads((root / path).read_text()) for _, path in PACKAGES}
    workspace = documents["Cargo.toml"].get("workspace", {}).get("package", {})
    found = {}
    for name, path in PACKAGES:
        package = documents[path].get("package", {})
        if package.get("name") != name:
            raise ReleaseError(f"{path} must declare package {name}")
        version = package.get("version")
        if version == {"workspace": True}:
            version = workspace.get("version")
        if not isinstance(version, str):
            raise ReleaseError(f"{path} has no concrete workspace/package version")
        stable_version(version)
        found[name] = version
    if len(set(found.values())) != 1:
        raise ReleaseError(f"Workspace versions disagree: {found}")
    return found["zchat"], documents


def bump_sources(bump: str, custom: str = "", root: Path = ROOT) -> str:
    current, documents = versions(root)
    selected = next_version(current, bump, custom)
    if selected == current:
        return selected
    names = {name for name, _ in PACKAGES}
    writes = {}
    for path, document in documents.items():
        if isinstance(document["package"].get("version"), str):
            document["package"]["version"] = selected
        inherited = document.get("workspace", {}).get("package", {})
        if "version" in inherited:
            inherited["version"] = selected
        for table in dependency_tables(document):
            for alias, declaration in table.items():
                if isinstance(declaration, dict) and declaration.get("package", alias) in names and "path" in declaration:
                    declaration["version"] = "=" + selected
        writes[root / path] = render_toml(document)
    distro = root / "zdistro.toml"
    if distro.is_file():
        document = tomllib.loads(distro.read_text())
        document["package"]["version"] = selected
        writes[distro] = render_toml(document)
    for path, content in writes.items():
        path.write_text(content, encoding="utf-8")
    return selected


def project_manifest(document: dict, locked: dict[tuple[str, str], str]) -> dict:
    result = copy.deepcopy(document)
    for dependencies in dependency_tables(result):
        for alias, declaration in dependencies.items():
            if not isinstance(declaration, dict) or "git" not in declaration:
                continue
            key = declaration.get("package", alias), repository(declaration["git"])
            if key not in locked:
                raise ReleaseError(f"No unique locked Project Z identity for {alias}")
            incompatible = {"version", "rev", "branch", "tag", "path", "registry", "registry-index", "workspace"}.intersection(declaration)
            if incompatible:
                raise ReleaseError(f"{alias}: Git development source contains selectors {sorted(incompatible)}")
            del declaration["git"]
            declaration["version"] = "=" + locked[key]
    return result


def request_bytes(url: str, limit: int = 8 * 1024 * 1024) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=30) as response:
        data = response.read(limit + 1)
    if len(data) > limit:
        raise ReleaseError(f"Response exceeds {limit} bytes: {url}")
    return data


def registry_version(name: str, version: str) -> dict | None:
    try:
        value = json.loads(request_bytes(f"https://crates.io/api/v1/crates/{name}/{version}"))["version"]
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise
    if value.get("num") != version or value.get("crate") != name or not re.fullmatch(r"[0-9a-f]{64}", value.get("checksum", "")):
        raise ReleaseError(f"Invalid registry identity/checksum for {name} {version}")
    if value.get("yanked"):
        raise ReleaseError(f"{name} {version} is yanked")
    return value


def registry_archive(name: str, version: str, cache: Path, metadata: dict | None = None) -> Path:
    metadata = metadata or registry_version(name, version)
    if not metadata:
        raise ReleaseError(f"Publish matching upstream package {name} {version} before zchat")
    filename = f"{name}-{version}.crate"
    cargo = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    candidates = [cache / filename, *sorted((cargo / "registry/cache").glob("*/" + filename))]
    for candidate in candidates:
        if candidate.is_file() and digest(candidate.read_bytes()) == metadata["checksum"]:
            return candidate
    payload = request_bytes(f"https://static.crates.io/crates/{name}/{filename}", 64 * 1024 * 1024)
    if digest(payload) != metadata["checksum"]:
        raise ReleaseError(f"{filename} does not match the crates.io index checksum")
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / filename
    temporary = destination.with_suffix(".download")
    temporary.write_bytes(payload)
    temporary.replace(destination)
    return destination


def archive_members(archive: Path, name: str, version: str) -> dict[str, bytes]:
    prefix = f"{name}-{version}/"
    output = {}
    total = 0
    with tarfile.open(archive, "r:gz") as source:
        for member in source:
            if member.isdir():
                continue
            path = member.name.removeprefix(prefix)
            parsed = PurePosixPath(path)
            if not member.isfile() or not member.name.startswith(prefix) or parsed.is_absolute() or ".." in parsed.parts or "\\" in path or path in output:
                raise ReleaseError(f"Unsafe or duplicate registry member: {member.name}")
            total += member.size
            if member.size > 32 * 1024 * 1024 or total > 128 * 1024 * 1024 or len(output) >= 20000:
                raise ReleaseError("Registry archive exceeds package limits")
            stream = source.extractfile(member)
            if stream is None:
                raise ReleaseError(f"Unreadable package member: {member.name}")
            output[path] = stream.read()
    return output


def documentation_path(path: str) -> bool:
    lower = path.lower()
    return lower.startswith("docs/") or (PurePosixPath(lower).parent == PurePosixPath(".") and any(lower.startswith(prefix) for prefix in ("readme", "changelog", "contributing", "security")) and lower.endswith((".md", ".rst", ".txt")))


def comparable_manifest(document: dict, documentation_tolerant: bool) -> dict:
    result = copy.deepcopy(document)
    result.pop("workspace", None)
    if documentation_tolerant:
        for field in ("authors", "description", "documentation", "homepage", "readme", "keywords", "categories", "include", "exclude"):
            result.get("package", {}).pop(field, None)
    return result


def compare_published(package: dict, manifest: dict, cache: Path, metadata: dict | None = None, *, documentation_tolerant: bool = False) -> dict:
    name, version = package["name"], package["version"]
    folder = Path(package["manifest_path"]).parent
    listing = set(run("cargo", "package", "--list", "--allow-dirty", "--manifest-path", str(folder / "Cargo.toml"), cwd=folder).splitlines())
    if not listing or any(PurePosixPath(item).is_absolute() or ".." in PurePosixPath(item).parts for item in listing):
        raise ReleaseError(f"Invalid Cargo publication scope for {name}")
    archive = registry_archive(name, version, cache, metadata)
    files = archive_members(archive, name, version)
    normalized = tomllib.loads(files.get("Cargo.toml", b"").decode())
    identity = normalized.get("package", {})
    expected_repo = package.get("repository") or manifest["package"].get("repository")
    if not isinstance(expected_repo, str):
        raise ReleaseError(f"{name}: missing repository identity")
    if (identity.get("name"), identity.get("version"), repository(identity.get("repository", ""))) != (name, version, repository(expected_repo)):
        raise ReleaseError(f"Registry package identity differs for {name} {version}")
    if "Cargo.toml.orig" not in files:
        raise ReleaseError(f"{name}: registry archive lacks original manifest")
    actual = comparable_manifest(tomllib.loads(files["Cargo.toml.orig"].decode()), documentation_tolerant)
    expected = comparable_manifest(manifest, documentation_tolerant)
    if actual != expected:
        raise ReleaseError(f"{name} {version}: registry manifest differs from the source projection")
    excluded = GENERATED | ({item for item in listing | files.keys() if documentation_path(item)} if documentation_tolerant else set())
    absent = sorted((listing - excluded) ^ (files.keys() - excluded))
    changed = [item for item, data in files.items() if item not in excluded and (not (folder / item).is_file() or (folder / item).read_bytes() != data)]
    if absent or changed:
        raise ReleaseError(f"{name} {version}: registry source differs at {', '.join((absent + changed)[:12])}")
    vcs = json.loads(files.get(".cargo_vcs_info.json", b"{}"))
    return {"package": name, "version": version, "repository": repository(expected_repo), "archive_sha256": digest(archive.read_bytes()), "files_compared": len(files.keys() - excluded), "published_git_sha": vcs.get("git", {}).get("sha1"), "documentation_differences_allowed": documentation_tolerant, "verification": "Cargo compilation/publication file scope, projected dependency/feature settings, source bytes and registry checksum"}


def clean_commit(root: Path = ROOT) -> str:
    if run("git", "status", "--porcelain", "--untracked-files=no", cwd=root).strip():
        raise ReleaseError("Commit source and lockfile changes before staging a release")
    commit = run("git", "rev-parse", "HEAD", cwd=root).strip()
    if not re.fullmatch(r"[0-9a-f]{40,64}", commit):
        raise ReleaseError("Source has no complete Git commit")
    return commit


def stage_registry(output: Path, cache: Path, root: Path = ROOT) -> dict:
    commit = clean_commit(root)
    version, _ = versions(root)
    output = output.resolve()
    if output.exists() or output.is_relative_to(root.resolve()):
        raise ReleaseError("Stage into a new directory outside the development checkout")
    lock_bytes = (root / "Cargo.lock").read_bytes()
    if run("git", "show", f"{commit}:Cargo.lock", cwd=root).encode() != lock_bytes:
        raise ReleaseError("Cargo.lock differs from the committed release")
    if any(line.startswith("160000 ") for line in run("git", "ls-tree", "-r", commit, cwd=root).splitlines()):
        raise ReleaseError("Git submodules require explicit recursive source staging")
    metadata = json.loads(run("cargo", "metadata", "--locked", "--all-features", "--format-version", "1", cwd=root))
    git_packages = [item for item in metadata["packages"] if (item.get("source") or "").startswith("git+")]
    identities = {}
    revisions = {}
    for package in git_packages:
        endpoint, separator, revision = package["source"][4:].rpartition("#")
        if not separator or "?" in endpoint or not re.fullmatch(r"[0-9a-f]{40,64}", revision) or not repository(endpoint).startswith("https://github.com/lightjunction/"):
            raise ReleaseError(f"{package['name']}: expected a locked Project Z default-branch Git source")
        key = package["name"], repository(endpoint)
        if key in identities:
            raise ReleaseError(f"Ambiguous Git source identity for {package['name']}")
        identities[key] = package["version"]
        revisions[package["id"]] = revision
    receipts = []
    for package in git_packages:
        source_dir = Path(package["manifest_path"]).parent
        if run("git", "rev-parse", "HEAD", cwd=source_dir).strip() != revisions[package["id"]] or run("git", "status", "--porcelain", "--untracked-files=no", cwd=source_dir).strip():
            raise ReleaseError(f"{package['name']}: Cargo checkout is not the immutable locked source")
        projected = project_manifest(tomllib.loads(Path(package["manifest_path"]).read_text()), identities)
        receipt = compare_published(package, projected, cache, documentation_tolerant=True)
        receipt["locked_git_sha"] = revisions[package["id"]]
        receipts.append(receipt)
    lock = tomllib.loads(lock_bytes.decode())
    checksums = {(item["package"], item["version"]): item["archive_sha256"] for item in receipts}
    replacements = {}
    for package in lock["package"]:
        if package.get("source", "").startswith("git+"):
            replacements[package["source"]] = REGISTRY
            package["source"] = REGISTRY
            package["checksum"] = checksums[package["name"], package["version"]]
    for package in lock["package"]:
        for index, dependency in enumerate(package.get("dependencies", [])):
            for old, new in replacements.items():
                dependency = dependency.replace(f"({old})", f"({new})")
            package["dependencies"][index] = dependency
    output.parent.mkdir(parents=True, exist_ok=True)
    output.mkdir()
    try:
        with tempfile.TemporaryFile() as temporary:
            subprocess.run(["git", "archive", "--format=tar", commit], cwd=root, stdout=temporary, check=True)
            temporary.seek(0)
            with tarfile.open(fileobj=temporary) as archive:
                archive.extractall(output, filter="data")
        for _, relative in PACKAGES:
            path = output / relative
            document = project_manifest(tomllib.loads(path.read_text()), identities)
            provenance = {"schema": "zchat.release-source.v1", "package": document["package"]["name"], "version": version, "source_commit": commit, "source_lock_sha256": digest(lock_bytes)}
            (path.parent / "release-source.json").write_text(json.dumps(provenance, indent=2) + "\n")
            if "include" in document["package"] and "release-source.json" not in document["package"]["include"]:
                document["package"]["include"].append("release-source.json")
            path.write_text(render_toml(document))
        (output / "Cargo.lock").write_text(render_toml(lock))
        projected = json.loads(run("cargo", "metadata", "--locked", "--all-features", "--format-version", "1", cwd=output))
        if any((item.get("source") or "").startswith("git+") for item in projected["packages"]):
            raise ReleaseError("A Git dependency remains in the registry projection")
        if sorted((item["name"], item["version"]) for item in metadata["packages"]) != sorted((item["name"], item["version"]) for item in projected["packages"]):
            raise ReleaseError("Registry projection changed the resolved package graph")
        source_paths = run("git", "ls-tree", "-r", "--name-only", commit, cwd=root).splitlines()
        source_paths.extend(str((Path(relative).parent / "release-source.json").as_posix()) for _, relative in PACKAGES)
        staged_files = {relative: digest((output / relative).read_bytes()) for relative in sorted(set(source_paths)) if (output / relative).is_file()}
        report = {"schema": "zchat.registry-stage.v1", "version": version, "source_commit": commit, "source_lock_sha256": digest(lock_bytes), "dependencies": receipts, "staged_files": staged_files, "status": "source-verified", "published": False}
        (output / "release-provenance.json").write_text(json.dumps(report, indent=2) + "\n")
        return report
    except BaseException:
        (output / "RELEASE-BLOCKED.txt").write_text("Staging failed. This directory must not be published.\n")
        raise


def publish_registry(stage: Path, cache: Path, dry_run: bool = False) -> dict:
    stage = stage.resolve()
    if (stage / "RELEASE-BLOCKED.txt").exists():
        raise ReleaseError("Staging is blocked")
    receipt = json.loads((stage / "release-provenance.json").read_text())
    if receipt.get("schema") != "zchat.registry-stage.v1" or receipt.get("status") != "source-verified":
        raise ReleaseError("No successfully verified registry stage")
    if not receipt.get("staged_files"):
        raise ReleaseError("Staging receipt has no source inventory")
    for relative, checksum in receipt["staged_files"].items():
        path = stage / relative
        if not path.is_file() or digest(path.read_bytes()) != checksum:
            raise ReleaseError(f"Staged source changed after verification: {relative}")
    version, documents = versions(stage)
    if version != receipt["version"]:
        raise ReleaseError("Staged version differs from its receipt")
    results = []
    published_archives = stage / "published-crates"
    published_archives.mkdir(exist_ok=True)
    for name, relative in PACKAGES:
        path = stage / relative
        expected = {"schema": "zchat.release-source.v1", "package": name, "version": version, "source_commit": receipt["source_commit"], "source_lock_sha256": receipt["source_lock_sha256"]}
        if json.loads((path.parent / "release-source.json").read_text()) != expected:
            raise ReleaseError(f"{name}: staged source provenance differs")
        package = {"name": name, "version": version, "manifest_path": str(path), "repository": "https://github.com/LIghtJUNction/zchat"}
        existing = registry_version(name, version)
        if existing:
            verified = compare_published(package, documents[relative], cache, existing)
            shutil.copyfile(registry_archive(name, version, cache, existing), published_archives / f"{name}-{version}.crate")
            results.append({"package": name, "state": "already-published-and-verified", **verified})
            continue
        command = ["cargo", "publish", "--locked", "--allow-dirty", "--manifest-path", str(path)]
        if dry_run:
            command.append("--dry-run")
        run(*command, cwd=stage)
        if dry_run:
            results.append({"package": name, "state": "dry-run-verified"})
            continue
        available = None
        for attempt in range(15):
            available = registry_version(name, version)
            if available:
                break
            time.sleep(2)
        if not available:
            raise ReleaseError(f"{name} uploaded but is not yet readable; rerun publish against this stage")
        verified = compare_published(package, documents[relative], cache, available)
        shutil.copyfile(registry_archive(name, version, cache, available), published_archives / f"{name}-{version}.crate")
        results.append({"package": name, "state": "published-and-verified", **verified})
        (stage / "publish-receipt.json").write_text(json.dumps({"source_commit": receipt["source_commit"], "version": version, "packages": results, "complete": False}, indent=2) + "\n")
    result = {"schema": "zchat.publish.v1", "source_commit": receipt["source_commit"], "version": version, "packages": results, "complete": not dry_run}
    (stage / "publish-receipt.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def release_checks(root: Path = ROOT):
    for command in (("cargo", "fmt", "--all", "--", "--check"), ("cargo", "test", "--workspace", "--locked", "--no-default-features"), (sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests", "-v")):
        output = run(*command, cwd=root)
        if output:
            print(output, end="")


def prepare_release(bump: str, custom: str, tag: str, push: bool, branch: str, root: Path = ROOT) -> dict:
    base = clean_commit(root)
    current, _ = versions(root)
    if tag:
        if tag != "v" + current or run("git", "rev-parse", tag + "^{commit}", cwd=root).strip() != base:
            raise ReleaseError("Release tag must match both the workspace version and checked-out commit")
        selected = current
    else:
        selected = bump_sources(bump, custom, root)
        if selected != current:
            run("cargo", "generate-lockfile", cwd=root)
        tag = "v" + selected
    release_checks(root)
    if selected != current:
        paths = [path for _, path in PACKAGES] + ["Cargo.lock", "zdistro.toml"]
        run("git", "add", "--", *paths, cwd=root)
        run("git", "-c", "user.name=github-actions[bot]", "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com", "commit", "-m", f"release: zchat {selected}", cwd=root)
    commit = clean_commit(root)
    tags = run("git", "tag", "--list", tag, cwd=root).splitlines()
    if tags:
        if run("git", "rev-parse", tag + "^{commit}", cwd=root).strip() != commit:
            raise ReleaseError(f"{tag} already identifies another commit")
    else:
        run("git", "-c", "user.name=github-actions[bot]", "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com", "tag", "-a", tag, "-m", f"zchat {selected}", cwd=root)
    if push:
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_./-]*", branch) or ".." in branch:
            raise ReleaseError("Invalid default branch")
        upstream = run("git", "ls-remote", "--exit-code", "origin", "refs/heads/" + branch, cwd=root).split()[0]
        if upstream not in (base, commit):
            raise ReleaseError("Default branch advanced during preparation; synchronize and rerun")
        run("git", "push", "--atomic", "origin", f"HEAD:refs/heads/{branch}", f"refs/tags/{tag}", cwd=root)
    return {"version": selected, "tag": tag, "commit": commit, "source_date_epoch": run("git", "show", "-s", "--format=%ct", commit, cwd=root).strip()}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("bump", "prepare"):
        item = sub.add_parser(name)
        item.add_argument("--bump", choices=("current", "patch", "minor", "major", "custom"), default="patch")
        item.add_argument("--version", default="")
        if name == "prepare":
            item.add_argument("--tag", default="")
            item.add_argument("--push", action="store_true")
            item.add_argument("--branch", default="main")
    staged = sub.add_parser("stage")
    staged.add_argument("--output", type=Path, required=True)
    staged.add_argument("--cache", type=Path, default=Path.home() / ".cache/zchat/release")
    publish = sub.add_parser("publish")
    publish.add_argument("--stage", type=Path, required=True)
    publish.add_argument("--cache", type=Path, default=Path.home() / ".cache/zchat/release")
    publish.add_argument("--dry-run", action="store_true")
    sub.add_parser("check-version")
    args = parser.parse_args()
    try:
        if args.command == "bump":
            result = {"version": bump_sources(args.bump, args.version)}
            run("cargo", "generate-lockfile")
        elif args.command == "prepare":
            result = prepare_release(args.bump, args.version, args.tag, args.push, args.branch)
            if path := os.environ.get("GITHUB_OUTPUT"):
                with open(path, "a", encoding="utf-8") as stream:
                    for key, value in result.items():
                        stream.write(f"{key}={value}\n")
        elif args.command == "stage":
            result = stage_registry(args.output, args.cache)
        elif args.command == "publish":
            result = publish_registry(args.stage, args.cache, args.dry_run)
        else:
            result = {"version": versions()[0]}
        print(json.dumps(result, indent=2))
        return 0
    except (ReleaseError, OSError, ValueError, KeyError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"Release failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
