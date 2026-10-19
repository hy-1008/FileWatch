"""Safety regressions; all fixtures are newly owned and retained for inspection."""

import unittest
from unittest.mock import patch

import package
from safe_test_dir import SafeTestDir


class FixtureSafetyTests(unittest.TestCase):
    def test_ownership_and_boundary_checks_precede_deletion(self):
        first = SafeTestDir("python-safety")
        other = SafeTestDir("python-safety")
        self.assertNotEqual(first.path, other.path)
        target = first.path / "fixture"
        target.write_bytes(b"owned")
        outside = other.path / "keep"
        outside.write_bytes(b"keep")
        with self.assertRaises(PermissionError):
            first.remove_file(outside)
        with self.assertRaises(PermissionError):
            first.remove_file(first.path / ".." / "data" / "fixture")
        first.marker.write_text("different owner", encoding="ascii")
        with self.assertRaises(PermissionError):
            first.remove_file(target)
        self.assertEqual(target.read_bytes(), b"owned")
        self.assertEqual(outside.read_bytes(), b"keep")
        first.marker.write_text(first.token, encoding="ascii")
        first.remove_file(target)
        self.assertFalse(target.exists())

    def test_link_to_another_owned_directory_is_refused(self):
        first = SafeTestDir("python-link")
        other = SafeTestDir("python-link-target")
        target = other.path / "keep"
        target.write_bytes(b"keep")
        link = first.path / "link"
        try:
            link.symlink_to(target)
        except OSError as error:
            if getattr(error, "winerror", None) == 1314 or isinstance(error, PermissionError):
                self.skipTest("This Windows account cannot create symlinks")
            raise
        with self.assertRaises(PermissionError):
            first.remove_file(link)
        self.assertEqual(target.read_bytes(), b"keep")

    def test_packaging_preserves_existing_directory_and_archive(self):
        owned = SafeTestDir("package-safety")
        with patch.object(package, "DIST", owned.path):
            first = package.reserve_output("0.1.0")
            fixture = first / "keep.txt"
            fixture.write_bytes(b"keep")
            second = package.reserve_output("0.1.0")
            self.assertNotEqual(first, second)
            with self.assertRaises(package.PackageError):
                package.reserve_output("0.1.0", first.name)
            with self.assertRaises(package.PackageError):
                package.reserve_output("0.1.0", "../outside")
            archive = package.make_zip(first)
            before = archive.read_bytes()
            with self.assertRaises(FileExistsError):
                package.make_zip(first)
            self.assertEqual(archive.read_bytes(), before)
            self.assertEqual(fixture.read_bytes(), b"keep")


if __name__ == "__main__":
    unittest.main()
