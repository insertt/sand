//! What sand knows about the sandboxes it manages. sbx does not report dynamic
//! mounts in a stable machine-readable form, so sand records what it applied.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::mounts::{Mode, MountSet};
use crate::paths::Dirs;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Project,
    /// Shared persistent scratch workspace (`--scratch`).
    Scratch,
    /// Throwaway mountless sandbox (`--dedicated-scratch`).
    Dedicated,
}

/// A mount sand manages itself (runtime dir, history) rather than one the user asked for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalMount {
    pub host: String,
    pub target: String,
    pub mode: Mode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxState {
    pub name: String,
    pub harness: String,
    pub agent: String,
    pub kind: Kind,
    pub mounts: MountSet,
    #[serde(default)]
    pub mcp: Vec<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub internal_mounts: Vec<InternalMount>,
    #[serde(default)]
    pub created_at: u64,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn load(dirs: &Dirs, name: &str) -> Result<Option<SandboxState>> {
    let file = dirs.state_file(name);
    match fs::read_to_string(&file) {
        Ok(text) => {
            let state = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
            Ok(Some(state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

pub fn save(dirs: &Dirs, state: &SandboxState) -> Result<()> {
    let file = dirs.state_file(&state.name);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(state)?).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &file).with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// Forgets a sandbox. History lives in its own subdirectory and is only
/// removed when `keep_history` is false.
pub fn forget(dirs: &Dirs, name: &str, keep_history: bool) -> Result<()> {
    let dir = dirs.sandbox_dir(name);
    if !dir.exists() {
        return Ok(());
    }
    if !keep_history {
        return fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()));
    }
    let history = dirs.history_dir(name);
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path == history {
            continue;
        }
        remove_any(&path)?;
    }
    Ok(())
}

pub fn remove_any(path: &Path) -> Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    };
    result.with_context(|| format!("removing {}", path.display()))
}

/// Turns arbitrary text into a valid sandbox name (letters, digits, hyphens,
/// periods; at least two characters; starts with a letter or digit).
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    match out.len() {
        0 => "sandbox".to_string(),
        1 => format!("{out}-sandbox"),
        _ => out,
    }
}

fn short_hash(text: &str) -> String {
    // FNV-1a: stable across runs and platforms, unlike the std hasher.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{:04x}", hash & 0xffff)
}

/// Default sandbox name for a project: `<harness>-<dirname>`, or
/// `<harness>-<dirname>-<hash>` when that name already belongs to another directory.
pub fn default_name(dirs: &Dirs, harness: &str, primary: &Path) -> Result<String> {
    let dirname = primary.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into());
    let base = sanitize_name(&format!("{harness}-{dirname}"));
    let hashed = format!("{base}-{}", short_hash(&primary.to_string_lossy()));
    let owns = |name: &str| -> Result<Option<bool>> {
        Ok(load(dirs, name)?.map(|s| s.mounts.primary.as_ref().is_some_and(|m| m.path == primary)))
    };
    if owns(&hashed)? == Some(true) {
        return Ok(hashed);
    }
    match owns(&base)? {
        Some(false) => Ok(hashed),
        _ => Ok(base),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::Mount;
    use std::path::PathBuf;

    fn dirs(root: &Path) -> Dirs {
        Dirs { home: root.join("home"), data: root.join("data"), config_file: root.join("config.toml") }
    }

    fn state(name: &str, primary: &str) -> SandboxState {
        SandboxState {
            name: name.into(),
            harness: "claude".into(),
            agent: "claude".into(),
            kind: Kind::Project,
            mounts: MountSet { primary: Some(Mount::rw(primary)), extras: vec![] },
            mcp: vec![],
            template: None,
            internal_mounts: vec![],
            created_at: 0,
        }
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("claude-My Project_2"), "claude-my-project-2");
        assert_eq!(sanitize_name("--x--"), "x-sandbox");
        assert_eq!(sanitize_name("???"), "sandbox");
        assert_eq!(sanitize_name("codex-.dotdir"), "codex-dotdir");
    }

    #[test]
    fn default_name_avoids_collisions_between_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let a = PathBuf::from("/x/proj");
        let b = PathBuf::from("/y/proj");
        assert_eq!(default_name(&dirs, "claude", &a).unwrap(), "claude-proj");
        save(&dirs, &state("claude-proj", "/x/proj")).unwrap();
        assert_eq!(default_name(&dirs, "claude", &a).unwrap(), "claude-proj");
        let hashed = default_name(&dirs, "claude", &b).unwrap();
        assert!(hashed.starts_with("claude-proj-") && hashed != "claude-proj");
        // Once the hashed sandbox exists it stays the answer even if the base name frees up.
        save(&dirs, &state(&hashed, "/y/proj")).unwrap();
        forget(&dirs, "claude-proj", false).unwrap();
        assert_eq!(default_name(&dirs, "claude", &b).unwrap(), hashed);
    }

    #[test]
    fn forget_can_keep_history() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        save(&dirs, &state("s1", "/p")).unwrap();
        fs::create_dir_all(dirs.history_dir("s1").join("projects")).unwrap();
        fs::create_dir_all(dirs.runtime_dir("s1")).unwrap();
        forget(&dirs, "s1", true).unwrap();
        assert!(load(&dirs, "s1").unwrap().is_none());
        assert!(dirs.history_dir("s1").join("projects").exists());
        assert!(!dirs.runtime_dir("s1").exists());
        forget(&dirs, "s1", false).unwrap();
        assert!(!dirs.sandbox_dir("s1").exists());
    }
}
