"""Release assembly must bind runtime receipts to the packaged native artifacts."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import github_release


class ReleaseAssetsTests(unittest.TestCase):
    commit = "a" * 40
    version = "0.1.0"

    def fixture(self, root):
        def write(name, value):
            (root / name).write_text(json.dumps(value))

        for system, arch in github_release.TARGETS:
            label = f"zchat-{self.version}-{system}-{arch}"
            payload = label.encode()
            digest = github_release.digest(payload)
            artifact = label + ".zip"
            (root / artifact).write_bytes(payload)
            receipt = artifact + ".receipt.json"
            write(receipt, {"status": "verified", "sha256": digest, "artifact": artifact})
            write(label + ".provenance.json", {"schema": "zchat.native-package.v1", "source_commit": self.commit, "version": self.version, "platform": system, "architecture": arch, "binary_sha256": digest, "plugin_sha256": digest, "artifacts": [{"file": artifact, "receipt": receipt, "bytes": len(payload), "sha256": digest}]})
            write(label + ".acceptance.json", {"schema": "zchat.acceptance.v1", "status": "passed", "mode": "loopback_matrix_only", "external_messages_sent": 0, "temporary_config_removed": True, "artifacts": {"binary": {"sha256": digest}, "plugin": {"sha256": digest}}})
        packages = []
        for name, _ in github_release.PACKAGES:
            payload = name.encode()
            (root / f"{name}-{self.version}.crate").write_bytes(payload)
            packages.append({"package": name, "archive_sha256": github_release.digest(payload)})
        write("publish-receipt.json", {"complete": True, "source_commit": self.commit, "version": self.version, "packages": packages})

    def test_complete_native_and_registry_evidence_is_accepted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            self.assertEqual(len(github_release.checked_assets(root, self.commit, self.version)), 29)

    def test_runtime_receipt_cannot_cover_another_plugin(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            receipt = root / "zchat-0.1.0-linux-x86_64.acceptance.json"
            value = json.loads(receipt.read_text())
            value["artifacts"]["plugin"]["sha256"] = "0" * 64
            receipt.write_text(json.dumps(value))
            with self.assertRaisesRegex(github_release.ReleaseError, "Accepted plugin differs"):
                github_release.checked_assets(root, self.commit, self.version)

    def test_missing_runtime_receipt_blocks_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            (root / "zchat-0.1.0-windows-aarch64.acceptance.json").unlink()
            with self.assertRaisesRegex(github_release.ReleaseError, "acceptance receipt is missing"):
                github_release.checked_assets(root, self.commit, self.version)

    def test_draft_recovery_includes_paginated_releases(self):
        draft = {"tag_name": "v0.1.0", "draft": True, "id": 100}
        with patch.object(github_release, "run", return_value=json.dumps([[{"tag_name": "v0.2.0"}], [draft]])):
            self.assertEqual(github_release.find_release("LIghtJUNction/zchat", "v0.1.0"), draft)
        with patch.object(github_release, "run", return_value=json.dumps([[draft], [draft]])):
            with self.assertRaisesRegex(github_release.ReleaseError, "More than one GitHub release"):
                github_release.find_release("LIghtJUNction/zchat", "v0.1.0")


if __name__ == "__main__":
    unittest.main()
