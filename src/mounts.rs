//! Mount specifications (`path[:ro|:rw]`) and reconciliation of the mounts an
//! existing sandbox has against the ones requested on the command line.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::{container_path, expand_tilde};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Rw,
    Ro,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Rw => "rw",
            Mode::Ro => "ro",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub path: PathBuf,
    pub mode: Mode,
}

impl fmt::Display for Mount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.path.display(), self.mode)
    }
}

/// Splits a trailing `:ro` / `:rw` off a spec. Only the suffix is inspected so
/// Windows drive letters (`C:\src:ro`) keep working. No suffix means read-write.
pub fn split_mode(spec: &str) -> (&str, Mode) {
    if let Some(path) = spec.strip_suffix(":ro") {
        (path, Mode::Ro)
    } else if let Some(path) = spec.strip_suffix(":rw") {
        (path, Mode::Rw)
    } else {
        (spec, Mode::Rw)
    }
}

/// Whether mounting `path` reaches into `protected`: it is that directory, one
/// of its ancestors or something inside it.
pub fn exposes(path: &Path, protected: &Path) -> bool {
    protected.starts_with(path) || path.starts_with(protected)
}

impl Mount {
    pub fn rw(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), mode: Mode::Rw }
    }

    /// Parses `path[:ro|:rw]`, resolving the path against `cwd` and `home`.
    pub fn parse(spec: &str, cwd: &Path, home: &Path) -> Result<Self> {
        let (raw, mode) = split_mode(spec);
        if raw.is_empty() {
            bail!("empty path in mount '{spec}'");
        }
        let joined = cwd.join(expand_tilde(raw, home));
        let path = dunce::canonicalize(&joined)
            .with_context(|| format!("mount path '{}' does not exist", joined.display()))?;
        Ok(Self { path, mode })
    }

    /// Resolves the path of `path[:ro|:rw]` like [`Mount::parse`], but also when
    /// it no longer exists (so that a deleted directory can still be unmounted).
    pub fn resolve_path(spec: &str, cwd: &Path, home: &Path) -> PathBuf {
        let joined = cwd.join(expand_tilde(split_mode(spec).0, home));
        dunce::canonicalize(&joined).unwrap_or(joined)
    }

    /// Workspace argument for `sbx create` / `sbx run`.
    pub fn create_arg(&self) -> OsString {
        let mut arg = self.path.clone().into_os_string();
        if self.mode == Mode::Ro {
            arg.push(":ro");
        }
        arg
    }

    /// `MOUNT_SPEC` for `sbx mount`: `HOST` or `HOST:CTR_TARGET:ro`.
    pub fn mount_spec(&self) -> OsString {
        let mut arg = self.path.clone().into_os_string();
        if self.mode == Mode::Ro {
            arg.push(":");
            arg.push(container_path(&self.path.to_string_lossy()));
            arg.push(":ro");
        }
        arg
    }

    /// `MOUNT_SPEC` for `sbx umount`.
    pub fn umount_spec(&self) -> OsString {
        self.path.clone().into_os_string()
    }
}

/// The user-visible mounts of a sandbox. The primary workspace is fixed when
/// the sandbox is created; extras can be changed with `sbx mount` / `sbx umount`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSet {
    pub primary: Option<Mount>,
    #[serde(default)]
    pub extras: Vec<Mount>,
}

impl MountSet {
    /// First mount becomes the primary workspace; later duplicates of a path
    /// override the mode of earlier ones.
    pub fn from_list(mounts: Vec<Mount>) -> Self {
        let mut set = MountSet::default();
        for mount in mounts {
            set.insert(mount);
        }
        set
    }

    fn insert(&mut self, mount: Mount) {
        match &mut self.primary {
            None => self.primary = Some(mount),
            Some(primary) if primary.path == mount.path => primary.mode = mount.mode,
            Some(_) => match self.extras.iter_mut().find(|m| m.path == mount.path) {
                Some(existing) => existing.mode = mount.mode,
                None => self.extras.push(mount),
            },
        }
    }

    pub fn all(&self) -> impl Iterator<Item = &Mount> {
        self.primary.iter().chain(self.extras.iter())
    }

    fn find(&self, path: &Path) -> Option<&Mount> {
        self.all().find(|m| m.path == path)
    }

    /// Equality that ignores the order of extra mounts.
    pub fn same_as(&self, other: &MountSet) -> bool {
        self.primary == other.primary
            && self.extras.len() == other.extras.len()
            && self.extras.iter().all(|m| other.extras.contains(m))
    }

    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        match &self.primary {
            Some(m) => lines.push(format!("{m}  [primary workspace]")),
            None => lines.push("(no primary workspace)".to_string()),
        }
        lines.extend(self.extras.iter().map(|m| m.to_string()));
        lines
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Make the sandbox's mounts exactly the requested ones.
    Replace,
    /// Keep the current mounts and add the requested ones.
    Merge,
}

/// What has to happen to move a sandbox from `current` to the mounts in `result`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub umount: Vec<Mount>,
    pub mount: Vec<Mount>,
    pub result: MountSet,
    /// Set when the change cannot be done in place (the primary workspace is
    /// baked in at creation time).
    pub recreate: Option<String>,
    pub notes: Vec<String>,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        self.umount.is_empty() && self.mount.is_empty() && self.recreate.is_none()
    }
}

pub fn plan(current: &MountSet, requested: &MountSet, strategy: Strategy) -> Plan {
    let mut plan = Plan::default();
    match strategy {
        Strategy::Replace => {
            plan.result = requested.clone();
            if current.primary != requested.primary {
                let show = |m: &Option<Mount>| m.as_ref().map_or("none".to_string(), |m| m.to_string());
                plan.recreate = Some(format!(
                    "the primary workspace changes from {} to {}",
                    show(&current.primary),
                    show(&requested.primary)
                ));
                return plan;
            }
            for mount in &current.extras {
                if !requested.extras.contains(mount) {
                    plan.umount.push(mount.clone());
                }
            }
            for mount in &requested.extras {
                if !current.extras.contains(mount) {
                    plan.mount.push(mount.clone());
                }
            }
        }
        Strategy::Merge => {
            plan.result = current.clone();
            for mount in requested.all() {
                match current.find(&mount.path) {
                    Some(existing) if existing.mode == mount.mode => {}
                    Some(existing) if current.primary.as_ref() == Some(existing) => plan.notes.push(format!(
                        "primary workspace stays {existing}; switching it to {} needs --recreate",
                        mount.mode
                    )),
                    Some(existing) => {
                        plan.umount.push(existing.clone());
                        plan.mount.push(mount.clone());
                        if let Some(slot) = plan.result.extras.iter_mut().find(|m| m.path == mount.path) {
                            slot.mode = mount.mode;
                        }
                    }
                    None => {
                        plan.mount.push(mount.clone());
                        plan.result.extras.push(mount.clone());
                    }
                }
            }
        }
    }
    plan
}

/// What has to happen to remove the extra mounts at `paths`. The primary
/// workspace is baked in at creation time and cannot be removed.
pub fn plan_umount(current: &MountSet, paths: &[PathBuf]) -> Result<Plan> {
    let mut plan = Plan { result: current.clone(), ..Plan::default() };
    for path in paths {
        if current.primary.as_ref().is_some_and(|p| &p.path == path) {
            bail!("{} is the primary workspace, which cannot be unmounted", path.display());
        }
        match current.extras.iter().find(|m| &m.path == path) {
            Some(mount) if !plan.umount.contains(mount) => {
                plan.umount.push(mount.clone());
                plan.result.extras.retain(|m| &m.path != path);
            }
            Some(_) => {}
            None => plan.notes.push(format!("{} is not mounted", path.display())),
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_reaching_sands_own_directories_are_found() {
        let data = Path::new("/home/me/.local/share/sand/sandboxes");
        for path in ["/", "/home/me", "/home/me/.local/share/sand", "/home/me/.local/share/sand/sandboxes/x/history"] {
            assert!(exposes(Path::new(path), data), "{path}");
        }
        for path in
            ["/home/me/dev/proj", "/home/me/.local/share/sand/scratch", "/home/me/.local/share/sand/sandboxes-old"]
        {
            assert!(!exposes(Path::new(path), data), "{path}");
        }
    }

    fn m(path: &str, mode: Mode) -> Mount {
        Mount { path: PathBuf::from(path), mode }
    }

    fn set(primary: &str, extras: &[(&str, Mode)]) -> MountSet {
        MountSet { primary: Some(m(primary, Mode::Rw)), extras: extras.iter().map(|(p, md)| m(p, *md)).collect() }
    }

    #[test]
    fn suffix_parsing() {
        assert_eq!(split_mode("../other:ro"), ("../other", Mode::Ro));
        assert_eq!(split_mode("../other:rw"), ("../other", Mode::Rw));
        assert_eq!(split_mode("."), (".", Mode::Rw));
        assert_eq!(split_mode("C:\\src:ro"), ("C:\\src", Mode::Ro));
        assert_eq!(split_mode("C:\\src"), ("C:\\src", Mode::Rw));
    }

    #[test]
    fn parse_resolves_and_requires_existence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        let cwd = root.join("a");
        assert_eq!(Mount::parse(".", &cwd, &root).unwrap(), m(cwd.to_str().unwrap(), Mode::Rw));
        assert_eq!(Mount::parse("..:ro", &cwd, &root).unwrap(), m(root.to_str().unwrap(), Mode::Ro));
        assert_eq!(Mount::parse("~/a:rw", &cwd, &root).unwrap(), m(cwd.to_str().unwrap(), Mode::Rw));
        assert!(Mount::parse("missing", &cwd, &root).is_err());
        assert!(Mount::parse(":ro", &cwd, &root).is_err());
    }

    #[test]
    fn sbx_argument_forms() {
        assert_eq!(m("/a/b", Mode::Rw).create_arg(), OsString::from("/a/b"));
        assert_eq!(m("/a/b", Mode::Ro).create_arg(), OsString::from("/a/b:ro"));
        assert_eq!(m("/a/b", Mode::Rw).mount_spec(), OsString::from("/a/b"));
        assert_eq!(m("/a/b", Mode::Ro).mount_spec(), OsString::from("/a/b:/a/b:ro"));
        assert_eq!(m("C:\\x", Mode::Ro).mount_spec(), OsString::from("C:\\x:/c/x:ro"));
        assert_eq!(m("/a/b", Mode::Ro).umount_spec(), OsString::from("/a/b"));
    }

    #[test]
    fn from_list_dedupes_and_picks_primary() {
        let s = MountSet::from_list(vec![m("/p", Mode::Rw), m("/a", Mode::Rw), m("/a", Mode::Ro), m("/p", Mode::Rw)]);
        assert_eq!(s, set("/p", &[("/a", Mode::Ro)]));
    }

    #[test]
    fn same_ignores_extra_order() {
        let a = set("/p", &[("/a", Mode::Ro), ("/b", Mode::Rw)]);
        let b = set("/p", &[("/b", Mode::Rw), ("/a", Mode::Ro)]);
        assert!(a.same_as(&b));
        assert!(!a.same_as(&set("/p", &[("/a", Mode::Rw), ("/b", Mode::Rw)])));
        assert!(!a.same_as(&set("/q", &[("/a", Mode::Ro), ("/b", Mode::Rw)])));
    }

    #[test]
    fn replace_unmounts_and_mounts_the_difference() {
        let current = set("/p", &[("/a", Mode::Ro), ("/b", Mode::Rw)]);
        let requested = set("/p", &[("/b", Mode::Ro), ("/c", Mode::Rw)]);
        let plan = plan(&current, &requested, Strategy::Replace);
        assert_eq!(plan.umount, vec![m("/a", Mode::Ro), m("/b", Mode::Rw)]);
        assert_eq!(plan.mount, vec![m("/b", Mode::Ro), m("/c", Mode::Rw)]);
        assert_eq!(plan.result, requested);
        assert!(plan.recreate.is_none());
    }

    #[test]
    fn replace_with_new_primary_needs_recreate() {
        let plan = plan(&set("/p", &[]), &set("/q", &[]), Strategy::Replace);
        assert!(plan.recreate.is_some());
        assert!(plan.mount.is_empty() && plan.umount.is_empty());
    }

    #[test]
    fn merge_keeps_current_and_adds_requested() {
        let current = set("/p", &[("/a", Mode::Ro)]);
        let requested = set("/q", &[("/a", Mode::Rw), ("/c", Mode::Ro)]);
        let plan = plan(&current, &requested, Strategy::Merge);
        assert_eq!(plan.umount, vec![m("/a", Mode::Ro)]);
        assert_eq!(plan.mount, vec![m("/q", Mode::Rw), m("/a", Mode::Rw), m("/c", Mode::Ro)]);
        assert_eq!(plan.result, set("/p", &[("/a", Mode::Rw), ("/q", Mode::Rw), ("/c", Mode::Ro)]));
        assert!(plan.recreate.is_none());
    }

    #[test]
    fn merge_never_changes_the_primary() {
        let current = set("/p", &[]);
        let requested = MountSet { primary: Some(m("/p", Mode::Ro)), extras: vec![] };
        let plan = plan(&current, &requested, Strategy::Merge);
        assert!(plan.is_noop());
        assert_eq!(plan.notes.len(), 1);
        assert_eq!(plan.result, current);
    }

    #[test]
    fn umount_removes_extras_only() {
        let current = set("/p", &[("/a", Mode::Ro), ("/b", Mode::Rw)]);
        let plan = plan_umount(&current, &[PathBuf::from("/a"), PathBuf::from("/a"), PathBuf::from("/c")]).unwrap();
        assert_eq!(plan.umount, vec![m("/a", Mode::Ro)]);
        assert!(plan.mount.is_empty());
        assert_eq!(plan.result, set("/p", &[("/b", Mode::Rw)]));
        assert_eq!(plan.notes, vec!["/c is not mounted".to_string()]);
        assert!(plan_umount(&current, &[PathBuf::from("/p")]).is_err());
        assert!(plan_umount(&current, &[PathBuf::from("/c")]).unwrap().is_noop());
    }

    #[test]
    fn resolve_path_works_for_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        assert_eq!(Mount::resolve_path(".:ro", &root, &root), root);
        assert_eq!(Mount::resolve_path("gone:ro", &root, &root), root.join("gone"));
    }

    #[test]
    fn identical_sets_are_a_noop() {
        let current = set("/p", &[("/a", Mode::Ro)]);
        assert!(plan(&current, &current.clone(), Strategy::Replace).is_noop());
        assert!(plan(&current, &current.clone(), Strategy::Merge).is_noop());
    }
}
