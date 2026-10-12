//! One session per sandbox. The lock is an OS file lock, so it is released
//! automatically if sand crashes or is killed.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::paths::Dirs;
use crate::state;

pub struct SessionLock {
    _file: File,
    info: PathBuf,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.info);
    }
}

/// Takes the session lock for `sandbox`, failing if another sand session holds it.
/// `description` is shown to whoever is locked out.
pub fn acquire(dirs: &Dirs, sandbox: &str, description: &str) -> Result<SessionLock> {
    let dir = dirs.locks_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{sandbox}.lock"));
    let info = dir.join(format!("{sandbox}.info"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let holder = fs::read_to_string(&info).unwrap_or_default();
            let holder =
                if holder.trim().is_empty() { "another sand process".to_string() } else { holder.trim().to_string() };
            bail!(
                "sandbox '{sandbox}' already has a running session ({holder}).\n\
                 Close that session first, or start a separate sandbox with --name."
            );
        }
        Err(TryLockError::Error(e)) => return Err(e).with_context(|| format!("locking {}", path.display())),
    }
    let _ =
        fs::write(&info, format!("pid {}, started at unix time {}, {description}", std::process::id(), state::now()));
    Ok(SessionLock { _file: file, info })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_session_is_rejected_until_the_first_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs =
            Dirs { home: tmp.path().into(), data: tmp.path().join("data"), config_file: tmp.path().join("c.toml") };
        let first = acquire(&dirs, "claude-proj", "in /x/proj").unwrap();
        let err = acquire(&dirs, "claude-proj", "elsewhere").err().expect("must be locked").to_string();
        assert!(err.contains("already has a running session"), "{err}");
        assert!(err.contains("in /x/proj"), "{err}");
        // A different sandbox is unaffected.
        let _other = acquire(&dirs, "codex-proj", "").unwrap();
        drop(first);
        acquire(&dirs, "claude-proj", "again").unwrap();
    }
}
