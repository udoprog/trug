//! A staging directory beside dist that replaces it in one step once a build has succeeded.

use std::io;
use std::path::{Path, PathBuf};

pub struct Staging {
    path: PathBuf,
    dist: PathBuf,
    done: bool,
}

/// The name prefix of every temporary directory made beside `dist`.
pub fn sibling_prefix(dist: &Path) -> String {
    let name = dist.file_name().unwrap_or_default().to_string_lossy();
    format!(".{name}.trug-")
}

fn sibling(dist: &Path, kind: &str) -> PathBuf {
    dist.with_file_name(format!(
        "{}{kind}-{}",
        sibling_prefix(dist),
        std::process::id()
    ))
}

/// Removes `dist` and the staging and old directories beside it; returns what was removed.
pub fn clean(dist: &Path) -> io::Result<Vec<PathBuf>> {
    let prefix = sibling_prefix(dist);
    let parent = dist.parent().unwrap_or(Path::new("."));
    let mut removed = Vec::new();
    if remove(dist)? {
        removed.push(dist.to_owned());
    }
    let mut siblings = Vec::new();
    match std::fs::read_dir(parent) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    siblings.push(entry.path());
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    siblings.sort();
    for path in siblings {
        if remove(&path)? {
            removed.push(path);
        }
    }
    Ok(removed)
}

/// Removes a directory (or a file or symlink) if it exists.
pub fn remove(path: &Path) -> io::Result<bool> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    result.map(|()| true)
}

impl Staging {
    /// Removes staging directories left by earlier runs and makes a fresh one.
    pub fn create(dist: &Path) -> io::Result<Self> {
        let prefix = sibling_prefix(dist);
        let parent = dist.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let path = sibling(dist, "staging");
        std::fs::create_dir(&path)?;
        Ok(Self {
            path,
            dist: dist.to_owned(),
            done: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Makes the staging directory the dist. An existing dist is exchanged with it atomically where
    /// the platform allows, so there is no moment without a complete dist.
    pub fn swap(mut self) -> io::Result<()> {
        if self.dist.exists() {
            if exchange(&self.path, &self.dist).is_err() {
                let old = sibling(&self.dist, "old");
                std::fs::rename(&self.dist, &old)?;
                if let Err(error) = std::fs::rename(&self.path, &self.dist) {
                    let _ = std::fs::rename(&old, &self.dist);
                    return Err(error);
                }
                let _ = std::fs::remove_dir_all(&old);
                self.done = true;
                return Ok(());
            }
            let _ = std::fs::remove_dir_all(&self.path);
        } else {
            std::fs::rename(&self.path, &self.dist)?;
        }
        self.done = true;
        Ok(())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(target_os = "linux")]
fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const RENAME_EXCHANGE: libc::c_uint = 2;
    let a = CString::new(a.as_os_str().as_bytes())?;
    let b = CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn exchange(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trug-dist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn swap_replaces_dist() {
        let root = temp("swap");
        let dist = root.join("dist");
        std::fs::create_dir(&dist).unwrap();
        std::fs::write(dist.join("old.txt"), "old").unwrap();
        let staging = Staging::create(&dist).unwrap();
        std::fs::write(staging.path().join("new.txt"), "new").unwrap();
        staging.swap().unwrap();
        assert!(dist.join("new.txt").exists());
        assert!(!dist.join("old.txt").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn clean_removes_dist_and_trug_siblings_only() {
        let root = temp("clean");
        let dist = root.join("dist");
        for name in [
            "dist",
            ".dist.trug-staging-1",
            ".dist.trug-old-2",
            "other",
            ".other.trug-staging-1",
        ] {
            std::fs::create_dir(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("f"), "x").unwrap();
        }
        let removed = clean(&dist).unwrap();
        assert_eq!(removed.len(), 3);
        assert!(!dist.exists());
        assert!(root.join("other").exists());
        assert!(root.join(".other.trug-staging-1").exists());
        assert!(clean(&dist).unwrap().is_empty());
    }

    #[test]
    fn clean_of_missing_parent_is_empty() {
        let root = temp("clean-missing");
        assert!(clean(&root.join("nope/dist")).unwrap().is_empty());
    }

    #[test]
    fn swap_creates_missing_dist() {
        let root = temp("create");
        let dist = root.join("dist");
        let staging = Staging::create(&dist).unwrap();
        std::fs::write(staging.path().join("a"), "a").unwrap();
        staging.swap().unwrap();
        assert!(dist.join("a").exists());
    }

    #[test]
    fn dropped_staging_leaves_dist_untouched() {
        let root = temp("fail");
        let dist = root.join("dist");
        std::fs::create_dir(&dist).unwrap();
        std::fs::write(dist.join("old.txt"), "old").unwrap();
        let staging = Staging::create(&dist).unwrap();
        std::fs::write(staging.path().join("partial"), "x").unwrap();
        drop(staging);
        assert_eq!(
            std::fs::read_to_string(dist.join("old.txt")).unwrap(),
            "old"
        );
        assert!(!dist.join("partial").exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn stale_staging_is_cleaned() {
        let root = temp("stale");
        let dist = root.join("dist");
        let stale = root.join(".dist.trug-staging-1");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("x"), "x").unwrap();
        let _staging = Staging::create(&dist).unwrap();
        assert!(!stale.exists());
    }
}
