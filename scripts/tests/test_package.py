"""Native target identity and install-layout checks."""
import importlib.util
from pathlib import Path
import struct
import sys
import tempfile
import tomllib
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("zchat_package", SCRIPTS / "package.py")
package = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package)


class IdentityTests(unittest.TestCase):
    def read(self, payload):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "binary"
            path.write_bytes(payload)
            return package.native_identity(path)

    def test_elf_and_macho_architectures(self):
        for machine, architecture in ((62, "x86_64"), (183, "aarch64")):
            elf = bytearray(64)
            elf[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<H", elf, 18, machine)
            self.assertEqual(self.read(elf), ("linux", architecture))
        for machine, architecture in ((0x1000007, "x86_64"), (0x100000C, "aarch64")):
            macho = b"\xcf\xfa\xed\xfe" + struct.pack("<I", machine) + b"\0" * 56
            self.assertEqual(self.read(macho), ("macos", architecture))

    def test_windows_checks_pe_header_and_machine(self):
        for machine, architecture in ((0x8664, "x86_64"), (0xAA64, "aarch64")):
            pe = bytearray(86)
            pe[:2] = b"MZ"
            struct.pack_into("<I", pe, 60, 80)
            pe[80:84] = b"PE\0\0"
            struct.pack_into("<H", pe, 84, machine)
            self.assertEqual(self.read(pe), ("windows", architecture))
        with self.assertRaises(package.ReleaseError):
            self.read(b"MZ" + b"\0" * 64)

    def test_text_and_32_bit_payload_are_rejected(self):
        with self.assertRaises(package.ReleaseError):
            self.read(b"#!/bin/sh\necho zchat\n")
        with self.assertRaises(package.ReleaseError):
            self.read(b"\x7fELF\x01\x01" + b"\0" * 58)


class LayoutTests(unittest.TestCase):
    def setUp(self):
        self.template = tomllib.loads((SCRIPTS.parent / "zdistro.toml").read_text())

    def test_linux_architecture_and_plugin_install_location(self):
        result = package.make_manifest(self.template, "linux", "aarch64", 123)
        self.assertEqual(result["targets"]["deb"]["architecture"], "arm64")
        self.assertEqual(result["targets"]["arch"]["architecture"], "aarch64")
        paths = {rule["destination"] for rule in result["targets"]["deb"]["files"]}
        self.assertIn("usr/bin/zchat", paths)
        self.assertIn("usr/lib/zchat/libzchat_providers.so", paths)
        self.assertEqual(self.template["targets"]["deb"]["architecture"], "amd64")

    def test_macos_preserves_executable_modes_in_bundle(self):
        result = package.make_manifest(self.template, "macos", "aarch64", 123)
        bundle = result["files"][0]
        self.assertEqual(bundle["destination"], "zchat.app")
        self.assertNotIn("mode", bundle)
        self.assertEqual(result["targets"]["pkg"]["files"][0]["destination"], "Applications/zchat.app")
        self.assertNotIn("msi", result["targets"])

    def test_windows_arm64_is_not_labeled_x64(self):
        result = package.make_manifest(self.template, "windows", "aarch64", 123)
        self.assertEqual(result["targets"]["msi"]["architecture"], "arm64")
        self.assertEqual(result["files"][0]["destination"], "zchat.exe")


if __name__ == "__main__":
    unittest.main()
