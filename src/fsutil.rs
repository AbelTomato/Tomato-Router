//! Small file-system helpers shared by configuration writers.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Return an unused sibling backup path for `path`.
pub fn next_backup_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let base = PathBuf::from(format!("{}.bak-tomato-{stamp}", path.display()));
    if !base.exists() {
        return base;
    }
    for suffix in 1..1000 {
        let candidate = PathBuf::from(format!("{}.bak-tomato-{stamp}-{suffix}", path.display()));
        if !candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(format!("{}.bak-tomato-{stamp}-overflow", path.display()))
}

/// Copy an existing file to a timestamped sibling backup.
///
/// Returns `None` when the source does not exist yet.
pub fn backup_file(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let backup = next_backup_path(path);
    fs::copy(path, &backup).with_context(|| {
        format!(
            "failed to back up `{}` to `{}`",
            path.display(),
            backup.display()
        )
    })?;
    Ok(Some(backup))
}

/// Replace a file atomically with same-directory temporary file plus rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file path has no parent directory")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("file name is not valid UTF-8")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temp = parent.join(format!(
        ".{file_name}.tomato-tmp-{}-{nonce}",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("failed to create temporary file `{}`", temp.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(error).context("failed to flush temporary file");
    }
    drop(file);

    #[cfg(windows)]
    {
        if let Err(error) = replace_windows(&temp, path) {
            // The WSL 9P redirector does not support MoveFileEx replacement.
            // Keep the safer failure behavior for ordinary Windows paths.
            if is_wsl_path(path) {
                if path.exists() {
                    fs::remove_file(path)?;
                }
                fs::rename(&temp, path)?;
            } else {
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
        }
    }
    #[cfg(not(windows))]
    fs::rename(&temp, path)?;

    Ok(())
}

#[cfg(windows)]
fn is_wsl_path(path: &Path) -> bool {
    let path = path.to_string_lossy().to_ascii_lowercase();
    path.starts_with(r"\\wsl$\") || path.starts_with(r"\\wsl.localhost\")
}

#[cfg(windows)]
fn replace_windows(source: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both vectors are NUL-terminated UTF-16 paths that remain alive
    // for the duration of the Windows API call.
    let ok = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        anyhow::bail!("MoveFileExW failed")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tomato-fsutil-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn atomic_write_replaces_file_and_backup_copies_old_content() {
        let path = temp_path("atomic");
        fs::write(&path, b"old").unwrap();
        let backup = backup_file(&path).unwrap().expect("backup");
        atomic_write(&path, b"new").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&backup);
    }

    #[cfg(windows)]
    #[test]
    fn recognizes_wsl_unc_paths() {
        assert!(is_wsl_path(Path::new(
            r"\\wsl.localhost\Ubuntu\home\user\.codex\config.toml"
        )));
        assert!(is_wsl_path(Path::new(
            r"\\wsl$\Ubuntu\home\user\.codex\config.toml"
        )));
        assert!(!is_wsl_path(Path::new(r"C:\Users\user\.codex\config.toml")));
    }
}
