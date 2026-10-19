//! Owned, isolated scratch directories for tests and the CLI self-test.
//!
//! A fresh directory is claimed with `create_dir` (never reused or pre-deleted).
//! The ownership marker stays outside `path()` so file-count assertions and
//! watchers only see the test's fixtures. Cleanup fails closed on changed
//! ownership, escaped paths, symlinks, or Windows junction/reparse points.

use std::fs::{self, Metadata, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
const MARKER: &str = ".filewatch-test-owner";

#[derive(Debug)]
pub struct TestDir {
    parent: PathBuf,
    root: PathBuf,
    data: PathBuf,
    display_path: PathBuf,
    token: String,
    cleaned: bool,
}

fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn ordinary(path: &Path) -> io::Result<Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    if metadata.file_type().is_symlink() || reparse {
        return Err(refused("test cleanup refuses symlinks and reparse points"));
    }
    Ok(metadata)
}

fn ordinary_ancestors(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        ordinary(ancestor)?;
    }
    Ok(())
}

fn display_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
        let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
        if units.starts_with(&unc) {
            let mut normal: Vec<u16> = r"\\".encode_utf16().collect();
            normal.extend_from_slice(&units[unc.len()..]);
            return PathBuf::from(OsString::from_wide(&normal));
        }
        if units.starts_with(&verbatim) {
            return PathBuf::from(OsString::from_wide(&units[verbatim.len()..]));
        }
    }
    path.to_path_buf()
}

impl TestDir {
    pub fn new(label: &str) -> io::Result<Self> {
        let parent = fs::canonicalize(std::env::temp_dir())?;
        ordinary_ancestors(&parent)?;
        let label: String = label
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(40)
            .collect();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..100 {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let token = format!("filewatch-test-{label}-{}-{now}-{id}", std::process::id());
            let root = parent.join(&token);
            match fs::create_dir(&root) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
                Ok(()) => {}
            }
            // If initialization fails, leave the new directory untouched;
            // there is no valid ownership proof yet, so cleanup is forbidden.
            ordinary(&root)?;
            if fs::canonicalize(&root)? != root {
                return Err(refused("new test directory escaped its canonical parent"));
            }
            let mut marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join(MARKER))?;
            marker.write_all(token.as_bytes())?;
            marker.sync_all()?;
            drop(marker);
            let data = root.join("data");
            fs::create_dir(&data)?;
            let display_path = display_path(&data);
            return Ok(Self {
                parent,
                root,
                data,
                display_path,
                token,
                cleaned: false,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not claim a unique test directory",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.display_path
    }

    fn verify_owner(&self) -> io::Result<()> {
        ordinary_ancestors(&self.root)?;
        if self.root.parent() != Some(self.parent.as_path())
            || fs::canonicalize(&self.root)? != self.root
            || fs::canonicalize(&self.parent)? != self.parent
        {
            return Err(refused(
                "test cleanup root/parent no longer matches its owner",
            ));
        }
        let marker = self.root.join(MARKER);
        if !ordinary(&marker)?.is_file() || fs::read_to_string(&marker)? != self.token {
            return Err(refused(
                "test cleanup ownership marker is missing or changed",
            ));
        }
        Ok(())
    }

    fn verify_child(&self, path: &Path) -> io::Result<Metadata> {
        self.verify_owner()?;
        if path == self.root || !path.starts_with(&self.root) {
            return Err(refused("test cleanup path is outside its owned directory"));
        }
        ordinary_ancestors(path)?;
        let resolved = fs::canonicalize(path)?;
        if resolved != path || !resolved.starts_with(&self.root) {
            return Err(refused("test cleanup path escaped its owned directory"));
        }
        ordinary(path)
    }

    /// Delete a fixture file only after verifying ownership and every ancestor.
    pub fn remove_file(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        let relative = relative.as_ref();
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(refused(
                "test deletion requires a plain relative fixture path",
            ));
        }
        let target = self.data.join(relative);
        if !self.verify_child(&target)?.is_file() {
            return Err(refused("test deletion requires a regular fixture file"));
        }
        fs::remove_file(target)
    }

    fn inspect_tree(&self, path: &Path, plan: &mut Vec<(PathBuf, bool)>) -> io::Result<()> {
        let metadata = self.verify_child(path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                self.inspect_tree(&entry?.path(), plan)?;
            }
        } else if !metadata.is_file() {
            return Err(refused("test cleanup refuses special files"));
        }
        plan.push((path.to_path_buf(), metadata.is_dir()));
        Ok(())
    }

    pub fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.verify_owner()?;
        let mut plan = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path != self.root.join(MARKER) {
                self.inspect_tree(&path, &mut plan)?;
            }
        }
        // Inspect the complete tree before deleting anything, and recheck each
        // entry immediately before removal. No recursive deletion API is used.
        for (path, is_dir) in plan {
            if self.verify_child(&path)?.is_dir() != is_dir {
                return Err(refused("test fixture changed type during cleanup"));
            }
            if is_dir {
                fs::remove_dir(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        self.verify_owner()?;
        fs::remove_file(self.root.join(MARKER))?;
        fs::remove_dir(&self.root)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(e) = self.cleanup() {
            eprintln!("Keeping test directory {}: {e}", self.root.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_labels_are_isolated_and_fixture_deletion_is_confined() {
        let mut first = TestDir::new("isolation").unwrap();
        let second = TestDir::new("isolation").unwrap();
        assert_ne!(first.path(), second.path());
        fs::write(first.path().join("fixture"), b"mine").unwrap();
        fs::write(second.path().join("fixture"), b"keep").unwrap();
        assert!(first.remove_file("../.filewatch-test-owner").is_err());
        assert!(first.remove_file(second.path().join("fixture")).is_err());
        first.remove_file("fixture").unwrap();
        first.cleanup().unwrap();
        assert_eq!(fs::read(second.path().join("fixture")).unwrap(), b"keep");
    }

    #[test]
    fn changed_ownership_refuses_cleanup_before_deleting_any_fixture() {
        let mut dir = TestDir::new("ownership").unwrap();
        let marker = dir.root.join(MARKER);
        fs::write(dir.path().join("keep"), b"keep").unwrap();
        fs::write(&marker, "different owner").unwrap();
        assert_eq!(
            dir.cleanup().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(dir.path().join("keep").exists());
        // Restore only this test's marker so its RAII guard can clean up.
        fs::write(marker, &dir.token).unwrap();
        dir.cleanup().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn symlink_fixture_prevents_cleanup_of_both_trees() {
        let mut dir = TestDir::new("link-refusal").unwrap();
        let outside = TestDir::new("link-target").unwrap();
        fs::write(outside.path().join("keep"), b"keep").unwrap();
        let link = dir.path().join("link");
        match std::os::windows::fs::symlink_file(outside.path().join("keep"), &link) {
            Ok(()) => {}
            Err(e)
                if e.raw_os_error() == Some(1314)
                    || e.kind() == io::ErrorKind::PermissionDenied =>
            {
                return
            }
            Err(e) => panic!("cannot create test symlink: {e}"),
        }
        assert_eq!(
            dir.cleanup().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"keep");
        // remove_file on the exact symlink created above removes only the link.
        fs::remove_file(link).unwrap();
        dir.cleanup().unwrap();
    }
}
