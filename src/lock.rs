//! One session per sandbox. The lock is an OS file lock, so it is released
//! automatically if sand crashes or is killed.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

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
            let holder = read_holder(&info);
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

/// Who holds the session lock for `sandbox`, or `None` when no session is running.
pub fn holder(dirs: &Dirs, sandbox: &str) -> Option<String> {
    let dir = dirs.locks_dir();
    is_held(&dir.join(format!("{sandbox}.lock"))).then(|| read_holder(&dir.join(format!("{sandbox}.info"))))
}

/// Held by a session that set `sandbox` up and handed it to Tern's chat, for
/// as long as the chat is open. Its own keep-alive keeps the sandbox running
/// meanwhile, so the `--acp` session Tern starts can skip the setup.
pub struct Handoff {
    _file: File,
}

pub fn hand_over(dirs: &Dirs, sandbox: &str) -> Result<Handoff> {
    let dir = dirs.locks_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{sandbox}.handoff"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock().with_context(|| format!("locking {}", path.display()))?;
    Ok(Handoff { _file: file })
}

/// Whether a session that set `sandbox` up is waiting on Tern's chat.
pub fn handed_over(dirs: &Dirs, sandbox: &str) -> bool {
    is_held(&dirs.locks_dir().join(format!("{sandbox}.handoff")))
}

fn is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().write(true).open(path) else { return false };
    matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

fn read_holder(info: &Path) -> String {
    let holder = fs::read_to_string(info).unwrap_or_default();
    if holder.trim().is_empty() { "another sand process".to_string() } else { holder.trim().to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_session_is_rejected_until_the_first_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs =
            Dirs { home: tmp.path().into(), data: tmp.path().join("data"), config_file: tmp.path().join("c.toml") };
        assert_eq!(holder(&dirs, "claude-proj"), None);
        let first = acquire(&dirs, "claude-proj", "in /x/proj").unwrap();
        assert!(holder(&dirs, "claude-proj").is_some_and(|h| h.ends_with("in /x/proj")));
        let err = acquire(&dirs, "claude-proj", "elsewhere").err().expect("must be locked").to_string();
        assert!(err.contains("already has a running session"), "{err}");
        assert!(err.contains("in /x/proj"), "{err}");
        // A different sandbox is unaffected.
        let _other = acquire(&dirs, "codex-proj", "").unwrap();
        drop(first);
        assert_eq!(holder(&dirs, "claude-proj"), None);
        acquire(&dirs, "claude-proj", "again").unwrap();
    }

    #[test]
    fn handoff_is_seen_only_while_held() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs =
            Dirs { home: tmp.path().into(), data: tmp.path().join("data"), config_file: tmp.path().join("c.toml") };
        assert!(!handed_over(&dirs, "claude-proj"));
        let handoff = hand_over(&dirs, "claude-proj").unwrap();
        assert!(handed_over(&dirs, "claude-proj") && !handed_over(&dirs, "codex-proj"));
        drop(handoff);
        assert!(!handed_over(&dirs, "claude-proj"));
    }
}
