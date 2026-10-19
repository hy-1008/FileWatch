"""Exclusive test fixtures. Never reuse, pre-delete, or recursively clean a path.

Outputs are intentionally retained for inspection. Only remove_file may delete a
fixture, after checking its owner, canonical boundary, and all ancestors.
"""

import os
from pathlib import Path
import secrets
import stat
import tempfile


class SafeTestDir:
    def __init__(self, label):
        self.parent = Path(tempfile.gettempdir()).resolve(strict=True)
        self._ordinary_ancestors(self.parent)
        label = "".join(c for c in label if c.isascii() and (c.isalnum() or c == "-"))[:40]
        # mkdtemp claims a fresh directory with atomic mkdir; existing names are
        # retried, never emptied. No cleanup is attempted on partial failure.
        self.root = Path(tempfile.mkdtemp(prefix="filewatch-test-" + label + "-", dir=self.parent))
        self.token = secrets.token_hex(32)
        self.marker = self.root / ".filewatch-test-owner"
        with self.marker.open("x", encoding="ascii") as stream:
            stream.write(self.token)
        self.path = self.root / "data"
        self.path.mkdir()
        self.verify_owner()

    @staticmethod
    def _ordinary(path):
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise PermissionError("Test deletion refuses symlinks/junctions/reparse points: " + str(path))
        return info

    @classmethod
    def _ordinary_ancestors(cls, path):
        for item in (path, *path.parents):
            cls._ordinary(item)

    def verify_owner(self):
        self._ordinary_ancestors(self.root)
        if self.root.parent != self.parent or self.root.resolve(strict=True) != self.root:
            raise PermissionError("Test directory escaped its canonical parent")
        if self.parent.resolve(strict=True) != self.parent:
            raise PermissionError("Test directory parent changed")
        if not stat.S_ISREG(self._ordinary(self.marker).st_mode):
            raise PermissionError("Test ownership marker is not a regular file")
        if self.marker.read_text(encoding="ascii") != self.token:
            raise PermissionError("Test ownership marker changed")

    def remove_file(self, path):
        self.verify_owner()
        target = Path(path)
        # Absolute fixture paths are accepted, but traversal and outside paths
        # are rejected before looking at the target or changing the filesystem.
        if not target.is_absolute() or ".." in target.parts:
            raise PermissionError("Test deletion requires an absolute fixture path without '..'")
        if target == self.path or not target.is_relative_to(self.path):
            raise PermissionError("Test deletion target is outside this run's fixtures")
        self._ordinary_ancestors(target)
        if target.resolve(strict=True) != target or not stat.S_ISREG(self._ordinary(target).st_mode):
            raise PermissionError("Test deletion target must be a canonical regular file")
        target.unlink()
