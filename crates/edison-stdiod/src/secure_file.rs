//! Private same-directory file replacement for credential and environment data.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("private file path has no parent"))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let (tmp, mut file) = create_temp(path)?;
    let result = (|| -> Result<()> {
        file.write_all(body)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        drop(file);
        replace(&tmp, path)
            .with_context(|| format!("replacing {} from {}", path.display(), tmp.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn create_temp(path: &Path) -> Result<(PathBuf, File)> {
    for _ in 0..32 {
        let tmp = unique_temp_path(path);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("creating {}", tmp.display())),
        }
    }
    Err(anyhow::anyhow!(
        "could not allocate a unique temporary file next to {}",
        path.display()
    ))
}

fn unique_temp_path(path: &Path) -> PathBuf {
    unique_sibling_path(path, "tmp")
}

fn unique_sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("private"));
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.{}.{}", std::process::id(), id, suffix));
    path.with_file_name(tmp_name)
}

#[cfg(unix)]
fn replace(tmp: &Path, path: &Path) -> io::Result<()> {
    std::fs::rename(tmp, path)
}

#[cfg(windows)]
fn replace(tmp: &Path, path: &Path) -> io::Result<()> {
    match std::fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
            ) =>
        {
            // std has no Windows equivalent of POSIX rename-over-existing.
            // Move the current file aside instead of deleting it so a crash
            // or failure between the two renames leaves the previous
            // contents on disk (as the backup) rather than losing the
            // credential outright; roll the backup straight back when
            // installing the new file fails. A brief window where the
            // destination is absent remains, but never one where no copy of
            // the data exists.
            let backup = unique_sibling_path(path, "bak");
            match std::fs::rename(path, &backup) {
                Ok(()) => {}
                // Destination vanished since the failed rename; retry directly.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return std::fs::rename(tmp, path);
                }
                Err(error) => return Err(error),
            }
            match std::fs::rename(tmp, path) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&backup);
                    Ok(())
                }
                Err(error) => {
                    let _ = std::fs::rename(&backup, path);
                    Err(error)
                }
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(any(unix, windows)))]
fn replace(tmp: &Path, path: &Path) -> io::Result<()> {
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "edison-stdiod-secure-file-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn temp_names_are_unique_and_in_destination_directory() {
        let path = tempdir().join("config.toml");
        let first = unique_temp_path(&path);
        let second = unique_temp_path(&path);
        assert_ne!(first, second);
        assert_eq!(first.parent(), path.parent());
        assert_eq!(second.parent(), path.parent());
    }

    #[test]
    fn repeated_replacement_leaves_no_temp_files() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        write_private(&path, b"first").unwrap();
        write_private(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_mode_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = tempdir().join("config.toml");
        write_private(&path, b"secret").unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
