"""Release gates exercise source identity and workspace version propagation."""
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import tomllib
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "release.py"
SPEC = importlib.util.spec_from_file_location("zchat_release", SCRIPT)
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class VersionTests(unittest.TestCase):
    def test_semantic_bumps_and_custom_order(self):
        self.assertEqual(release.next_version("1.9.8", "minor"), "1.10.0")
        self.assertEqual(release.next_version("1.9.8", "major"), "2.0.0")
        self.assertEqual(release.next_version("0.1.0", "current"), "0.1.0")
        self.assertEqual(release.next_version("0.1.0", "custom", "0.2.0"), "0.2.0")
        for value in ("01.2.3", "1.2", "v1.2.3", "1.2.3-dev", "1.2.3\n"):
            with self.assertRaises(release.ReleaseError):
                release.stable_version(value)
        with self.assertRaises(release.ReleaseError):
            release.next_version("1.2.3", "custom", "1.2.3")

    def test_bump_updates_local_packages_without_registry_selectors_on_git(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name, relative in release.PACKAGES:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(f'[package]\nname="{name}"\nversion="0.1.0"\n[dependencies]\nzconv={{git="https://github.com/LIghtJUNction/zconv.git"}}\nzchat-core={{path="../zchat-core",version="0.1.0"}}\n')
            (root / "zdistro.toml").write_text('[package]\nname="zchat"\nversion="0.1.0"\n')
            self.assertEqual(release.bump_sources("minor", root=root), "0.2.0")
            for _, relative in release.PACKAGES:
                document = tomllib.loads((root / relative).read_text())
                self.assertEqual(document["package"]["version"], "0.2.0")
                self.assertEqual(document["dependencies"]["zchat-core"]["version"], "=0.2.0")
                self.assertNotIn("version", document["dependencies"]["zconv"])
            self.assertEqual(tomllib.loads((root / "zdistro.toml").read_text())["package"]["version"], "0.2.0")

    def test_inconsistent_workspace_stops_before_writes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for index, (name, relative) in enumerate(release.PACKAGES):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(f'[package]\nname="{name}"\nversion="0.1.{index}"\n')
            before = (root / "Cargo.toml").read_bytes()
            with self.assertRaises(release.ReleaseError):
                release.bump_sources("patch", root=root)
            self.assertEqual((root / "Cargo.toml").read_bytes(), before)


class ProjectionTests(unittest.TestCase):
    def test_documentation_tolerance_preserves_dependency_and_feature_checks(self):
        original = {"package": {"name": "x", "description": "old", "license": "MIT", "include": ["src/**", "docs/**"]}, "dependencies": {"zlib": {"version": "=1.0.0"}}, "features": {"std": []}}
        changed = {"package": {"name": "x", "description": "new", "license": "MIT", "include": ["src/**"]}, "dependencies": {"zlib": {"version": "=1.0.0"}}, "features": {"std": []}}
        self.assertEqual(release.comparable_manifest(original, True), release.comparable_manifest(changed, True))
        self.assertNotEqual(release.comparable_manifest(original, False), release.comparable_manifest(changed, False))
        changed["dependencies"]["zlib"]["version"] = "=2.0.0"
        self.assertNotEqual(release.comparable_manifest(original, True), release.comparable_manifest(changed, True))
        self.assertTrue(release.documentation_path("docs/releasing.md"))
        self.assertFalse(release.documentation_path("native/shader.glsl"))
        self.assertFalse(release.documentation_path("LICENSE.txt"))

    def test_projection_covers_alias_target_build_and_workspace(self):
        source = {"dependencies": {"alias": {"package": "zlib", "git": "https://github.com/LIghtJUNction/zlib.git", "features": ["std"]}}, "target": {"cfg(windows)": {"build-dependencies": {"zlib": {"git": "https://github.com/LIghtJUNction/zlib.git"}}}}, "workspace": {"dependencies": {"zlib": {"git": "https://github.com/LIghtJUNction/zlib.git"}}}}
        identity = {("zlib", "https://github.com/lightjunction/zlib"): "1.2.3"}
        projected = release.project_manifest(source, identity)
        self.assertEqual(projected["dependencies"]["alias"]["version"], "=1.2.3")
        self.assertEqual(projected["target"]["cfg(windows)"]["build-dependencies"]["zlib"]["version"], "=1.2.3")
        self.assertEqual(projected["workspace"]["dependencies"]["zlib"]["version"], "=1.2.3")
        self.assertIn("git", source["dependencies"]["alias"])

    def test_selector_and_unknown_source_are_rejected(self):
        identity = {("zlib", "https://github.com/lightjunction/zlib"): "1.2.3"}
        for selector in ("version", "rev", "tag", "branch", "path", "registry"):
            source = {"dependencies": {"zlib": {"git": "https://github.com/LIghtJUNction/zlib.git", selector: "x"}}}
            with self.assertRaises(release.ReleaseError):
                release.project_manifest(source, identity)
        with self.assertRaises(release.ReleaseError):
            release.project_manifest({"dependencies": {"other": {"git": "https://github.com/third/other.git"}}}, identity)

    def test_roundtrip_preserves_cargo_lock_arrays_and_target_keys(self):
        document = {"version": 4, "package": [{"name": "foo", "version": "1.2.3", "dependencies": ["bar 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)"]}], "target": {"cfg(target_os = \"linux\")": {"dependencies": {"zlib": {"default-features": False}}}}}
        self.assertEqual(tomllib.loads(release.render_toml(document)), document)


class ArchiveTests(unittest.TestCase):
    def archive(self, root, files):
        output = root / "demo-1.0.0.crate"
        with tarfile.open(output, "w:gz") as archive:
            for name, data in files.items():
                member = tarfile.TarInfo("demo-1.0.0/" + name)
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
        return output

    def test_rejects_escape_path(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = self.archive(root, {"../outside": b"x"})
            with self.assertRaises(release.ReleaseError):
                release.archive_members(archive, "demo", "1.0.0")

    def test_source_identity_detects_new_or_modified_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            (root / "src/lib.rs").write_text("pub fn ok() {}\n")
            document = {"package": {"name": "demo", "version": "1.0.0", "repository": "https://github.com/LIghtJUNction/demo"}}
            manifest = release.render_toml(document).encode()
            (root / "Cargo.toml").write_bytes(manifest)
            archive = self.archive(root, {"Cargo.toml": manifest, "Cargo.toml.orig": manifest, "src/lib.rs": (root / "src/lib.rs").read_bytes()})
            package = {"name": "demo", "version": "1.0.0", "manifest_path": str(root / "Cargo.toml"), "repository": document["package"]["repository"]}
            with patch.object(release, "registry_archive", return_value=archive), patch.object(release, "run", return_value="Cargo.toml\nCargo.toml.orig\nsrc/lib.rs\n"):
                result = release.compare_published(package, document, root)
                self.assertEqual(result["files_compared"], 1)
                (root / "src/lib.rs").write_text("pub fn changed() {}\n")
                with self.assertRaises(release.ReleaseError):
                    release.compare_published(package, document, root)
            (root / "src/lib.rs").write_text("pub fn ok() {}\n")
            with patch.object(release, "registry_archive", return_value=archive), patch.object(release, "run", return_value="Cargo.toml\nCargo.toml.orig\nsrc/lib.rs\nsrc/new.rs\n"):
                with self.assertRaises(release.ReleaseError):
                    release.compare_published(package, document, root)

    def test_publish_rejects_mutated_stage_before_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            source = root / "src/lib.rs"
            source.write_text("original")
            receipt = {"schema": "zchat.registry-stage.v1", "status": "source-verified", "staged_files": {"src/lib.rs": release.digest(source.read_bytes())}}
            (root / "release-provenance.json").write_text(json.dumps(receipt))
            source.write_text("modified")
            with patch.object(release, "registry_version") as network:
                with self.assertRaises(release.ReleaseError):
                    release.publish_registry(root, root / "cache")
                network.assert_not_called()


if __name__ == "__main__":
    unittest.main()
