#!/usr/bin/env python3
"""Publish the six verified desktop targets and registry receipts as one release."""
import argparse
import json
from pathlib import Path
import re
import sys
import tempfile

from release import PACKAGES, ROOT, ReleaseError, digest, run, versions

TARGETS = {(system, architecture) for system in ("linux", "macos", "windows") for architecture in ("x86_64", "aarch64")}


def find_release(repository_name: str, tag: str) -> dict | None:
    # The tag endpoint only returns published releases. The authenticated list
    # also includes drafts, which makes interrupted upload/publish resumable.
    pages = json.loads(run("gh", "api", "--paginate", "--slurp", f"repos/{repository_name}/releases?per_page=100"))
    matches = [item for page in pages for item in page if item.get("tag_name") == tag]
    if len(matches) > 1:
        raise ReleaseError("More than one GitHub release identifies the requested tag")
    return matches[0] if matches else None


def checked_assets(directory: Path, commit: str, version: str) -> list[Path]:
    files = [path for path in directory.rglob("*") if path.is_file()]
    names = [path.name for path in files]
    if len(names) != len(set(names)):
        raise ReleaseError("Release assets have colliding filenames")
    by_name = {path.name: path for path in files}
    found = set()
    for path in files:
        if not path.name.endswith(".provenance.json"):
            continue
        data = json.loads(path.read_text())
        if data.get("schema") != "zchat.native-package.v1":
            continue
        if data.get("source_commit") != commit or data.get("version") != version:
            raise ReleaseError(f"Native artifact source/version differs: {path.name}")
        target = data["platform"], data["architecture"]
        if target in found or target not in TARGETS:
            raise ReleaseError("Duplicate or unexpected native release target")
        found.add(target)
        acceptance_file = by_name.get(f"zchat-{version}-{target[0]}-{target[1]}.acceptance.json")
        if acceptance_file is None:
            raise ReleaseError(f"Native runtime acceptance receipt is missing: {target}")
        acceptance = json.loads(acceptance_file.read_text())
        if acceptance.get("schema") != "zchat.acceptance.v1" or acceptance.get("status") != "passed" or acceptance.get("mode") != "loopback_matrix_only" or acceptance.get("external_messages_sent") != 0 or not acceptance.get("temporary_config_removed"):
            raise ReleaseError(f"Native runtime acceptance failed: {target}")
        for name in ("binary", "plugin"):
            if acceptance.get("artifacts", {}).get(name, {}).get("sha256") != data.get(f"{name}_sha256"):
                raise ReleaseError(f"Accepted {name} differs from packaged native artifact: {target}")
        if not data.get("artifacts"):
            raise ReleaseError("Native target has no verified artifacts")
        for item in data["artifacts"]:
            artifact = by_name.get(item["file"])
            receipt_file = by_name.get(item["receipt"])
            if artifact is None or receipt_file is None or artifact.stat().st_size != item["bytes"] or digest(artifact.read_bytes()) != item["sha256"]:
                raise ReleaseError(f"Missing or modified release artifact: {item['file']}")
            receipt = json.loads(receipt_file.read_text())
            if receipt.get("status") != "verified" or receipt.get("sha256") != item["sha256"] or receipt.get("artifact") != artifact.name:
                raise ReleaseError(f"Native receipt differs: {receipt_file.name}")
    if found != TARGETS:
        raise ReleaseError(f"Native targets missing: {sorted(TARGETS - found)}")
    publication = by_name.get("publish-receipt.json")
    if publication is None:
        raise ReleaseError("Registry publish receipt is missing")
    receipt = json.loads(publication.read_text())
    if not receipt.get("complete") or receipt.get("source_commit") != commit or receipt.get("version") != version:
        raise ReleaseError("Registry publication is incomplete or belongs to another source")
    if {item["package"] for item in receipt.get("packages", [])} != {name for name, _ in PACKAGES}:
        raise ReleaseError("Registry receipt does not cover every workspace package")
    for item in receipt["packages"]:
        archive = by_name.get(f"{item['package']}-{version}.crate")
        if archive is None or digest(archive.read_bytes()) != item["archive_sha256"]:
            raise ReleaseError(f"Published crate archive is missing or differs: {item['package']}")
    return sorted(files)


def publish(tag: str, commit: str, directory: Path):
    version, _ = versions()
    if tag != "v" + version or not re.fullmatch(r"[0-9a-f]{40,64}", commit) or run("git", "rev-parse", "HEAD").strip() != commit:
        raise ReleaseError("Tag, version and release checkout differ")
    references = run("git", "ls-remote", "--tags", "origin", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}")
    remote = dict(line.split()[::-1] for line in references.splitlines())
    if remote.get(f"refs/tags/{tag}^{{}}", remote.get(f"refs/tags/{tag}")) != commit:
        raise ReleaseError("Remote release tag identifies another commit")
    assets = checked_assets(directory.resolve(), commit, version)
    repository_name = "LIghtJUNction/zchat"
    existing = find_release(repository_name, tag)
    if existing is None:
        notes = f"# zchat {version}\n\nNative Linux, macOS and Windows packages for x86_64 and ARM64, built from `{commit}`.\n\nEach native package includes its zdistro manifest, verification receipt and source provenance. The four crates.io packages include the release source identity.\n\nSee [README](https://github.com/LIghtJUNction/zchat/tree/{tag}) for backend requirements and [release operations](https://github.com/LIghtJUNction/zchat/blob/{tag}/docs/releasing.md) for installation and verification scope.\n\nDesktop packages are unsigned. Protocol engines and account credentials are configured separately.\n"
        with tempfile.TemporaryDirectory(prefix="zchat-release-notes-") as temporary:
            path = Path(temporary) / "notes.md"
            path.write_text(notes)
            run("gh", "release", "create", tag, "--repo", repository_name, "--verify-tag", "--draft", "--title", f"zchat {version}", "--notes-file", str(path))
    else:
        if existing.get("tag_name") != tag:
            raise ReleaseError("Existing GitHub release does not identify the requested tag")
    for index in range(0, len(assets), 20):
        run("gh", "release", "upload", tag, "--repo", repository_name, "--clobber", *(str(path) for path in assets[index:index + 20]))
    uploaded = find_release(repository_name, tag)
    if uploaded is None:
        raise ReleaseError("GitHub release disappeared during asset upload")
    remote_assets = {item["name"]: item for item in uploaded.get("assets", [])}
    for path in assets:
        remote_asset = remote_assets.get(path.name, {})
        if remote_asset.get("state") != "uploaded" or remote_asset.get("size") != path.stat().st_size:
            raise ReleaseError(f"GitHub asset upload did not complete: {path.name}")
        if remote_asset.get("digest") and remote_asset["digest"] != "sha256:" + digest(path.read_bytes()):
            raise ReleaseError(f"GitHub asset digest differs: {path.name}")
    if uploaded.get("draft"):
        run("gh", "release", "edit", tag, "--repo", repository_name, "--draft=false")
    print(run("gh", "release", "view", tag, "--repo", repository_name, "--json", "url,tagName,assets"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--assets-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        publish(args.tag, args.commit, args.assets_dir)
    except (ReleaseError, OSError, ValueError, KeyError) as error:
        print(f"GitHub release failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
