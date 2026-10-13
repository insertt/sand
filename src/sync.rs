//! Copies the host's harness configuration (settings, commands, plugins, …)
//! into the sandbox. Files are staged in the sandbox's runtime directory, which is
//! mounted read-only at `~/.sand`, and applied by a small script inside the sandbox.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};

use crate::config::SettingsFormat;
use crate::harness::Profile;
use crate::paths::{RUNTIME_DIR_NAME, expand_tilde};

/// Recursively merges `over` into `base`; objects merge, everything else is replaced.
pub fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, over) => *base = over,
    }
}

fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            map.values_mut().for_each(strip_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

fn parse_settings(text: &str, format: SettingsFormat, what: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value = match format {
        SettingsFormat::Json => jsonc_parser::parse_to_serde_value::<Value>(text, &Default::default())
            .map_err(|e| anyhow!("{what} is not valid JSON: {e}"))?,
        SettingsFormat::Toml => {
            let table: toml::Table = toml::from_str(text).map_err(|e| anyhow!("{what} is not valid TOML: {e}"))?;
            serde_json::to_value(table)?
        }
    };
    Ok(if value.is_object() { value } else { Value::Object(Map::new()) })
}

fn render_settings(mut value: Value, format: SettingsFormat) -> Result<String> {
    Ok(match format {
        SettingsFormat::Json => format!("{}\n", serde_json::to_string_pretty(&value)?),
        SettingsFormat::Toml => {
            strip_nulls(&mut value);
            let table: toml::Table = serde_json::from_value(value).context("converting merged settings to TOML")?;
            toml::to_string(&table)?
        }
    })
}

const SECRET_MARKERS: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"];

/// Removes entries of a top-level `env` table whose names look like credentials
/// (`ANTHROPIC_API_KEY`, `GITHUB_TOKEN`, …) and returns the removed names.
/// Credentials belong in `sbx secret`, which injects them outside the sandbox.
fn strip_secret_env(settings: &mut Value) -> Vec<String> {
    let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) else { return Vec::new() };
    let secret: Vec<String> = env
        .keys()
        .filter(|name| SECRET_MARKERS.iter().any(|marker| name.to_ascii_uppercase().contains(marker)))
        .cloned()
        .collect();
    for name in &secret {
        env.remove(name);
    }
    secret
}

/// Merged settings plus the names of host `env` entries withheld as credentials.
pub struct Merged {
    pub content: String,
    pub withheld_env: Vec<String>,
}

/// Merges settings in precedence order: sandbox defaults < host settings < sand overlay.
pub fn merge_settings(base: &str, host: Option<&str>, profile: &Profile) -> Result<Merged> {
    let format = profile.settings_format;
    let mut merged = parse_settings(base, format, "the sandbox's settings file")?;
    let mut withheld_env = Vec::new();
    if let Some(host) = host {
        let mut host = parse_settings(host, format, "the host settings file")?;
        if let Value::Object(map) = &mut host {
            for key in &profile.strip_keys {
                map.remove(key);
            }
        }
        withheld_env = strip_secret_env(&mut host);
        deep_merge(&mut merged, host);
    }
    deep_merge(&mut merged, profile.overlay.clone());
    Ok(Merged { content: render_settings(merged, format)?, withheld_env })
}

/// Host path prefixes and the sandbox paths they are copied to.
pub type PathMap = Vec<(String, String)>;

/// Rewrites absolute host paths that point into copied configuration (for
/// example plugin install paths) to their location inside the sandbox.
/// Workspace paths are left alone: sbx mounts those at identical paths.
pub fn rewrite_paths(text: &str, map: &PathMap) -> String {
    let mut variants: Vec<(String, &str)> = Vec::new();
    for (host, sandbox) in map {
        if host == sandbox || host.is_empty() {
            continue;
        }
        for form in [host.replace('\\', "\\\\"), host.replace('\\', "/"), host.clone()] {
            if !variants.iter().any(|(v, _)| *v == form) {
                variants.push((form, sandbox));
            }
        }
    }
    variants.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        for (from, to) in &variants {
            if let Some(tail) = rest.strip_prefix(from.as_str()) {
                let boundary =
                    tail.chars().next().is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'));
                if boundary {
                    out.push_str(to);
                    // Remaining separators of a Windows path become forward slashes.
                    let end = tail.find(|c: char| c == '"' || c == '\'' || c.is_whitespace()).unwrap_or(tail.len());
                    out.push_str(&tail[..end].replace("\\\\", "/").replace('\\', "/"));
                    rest = &tail[end..];
                    continue 'outer;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn rel_join(dir: &str, entry: &str) -> String {
    format!("{}/{}", dir.trim_matches('/'), entry.trim_matches('/'))
}

/// Absolute sandbox path of something relative to the sandbox home.
fn in_sandbox(sandbox_home: &str, rel: &str) -> String {
    format!("{}/{}", sandbox_home.trim_end_matches('/'), rel.trim_matches('/'))
}

/// Host config dir resolved against the home directory.
pub fn host_config_dir(profile: &Profile, host_home: &Path) -> Option<PathBuf> {
    profile.host_config_dir.as_deref().map(|dir| expand_tilde(dir, host_home))
}

pub fn path_map(profile: &Profile, host_home: &Path, sandbox_home: &str) -> PathMap {
    let mut map = PathMap::new();
    if let (Some(host), Some(sandbox)) = (host_config_dir(profile, host_home), &profile.sandbox_config_dir) {
        map.push((host.to_string_lossy().into_owned(), in_sandbox(sandbox_home, sandbox)));
    }
    for pair in &profile.extra_copy {
        let host = expand_tilde(&pair.from, host_home);
        map.push((host.to_string_lossy().into_owned(), in_sandbox(sandbox_home, &pair.to)));
    }
    map
}

const REWRITE_EXTENSIONS: &[&str] = &["json", "jsonc", "toml", "yaml", "yml"];
const REWRITE_MAX_BYTES: u64 = 1024 * 1024;
const MAX_DEPTH: usize = 40;

struct Stager<'a> {
    exclude: &'a [String],
    map: &'a PathMap,
    home_out: &'a Path,
    manifest: BTreeSet<String>,
    copied: usize,
}

impl Stager<'_> {
    /// Copies `src` (following symlinks) to `rel` below the staged home directory.
    fn copy(&mut self, src: &Path, rel: &str, depth: usize) -> Result<()> {
        // Broken symlinks and unreadable entries are skipped rather than fatal.
        let Ok(meta) = fs::metadata(src) else { return Ok(()) };
        if meta.is_dir() {
            if depth >= MAX_DEPTH {
                return Ok(());
            }
            let mut entries: Vec<_> = fs::read_dir(src)
                .with_context(|| format!("reading {}", src.display()))?
                .filter_map(|e| e.ok())
                .collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                if self.exclude.contains(&name) {
                    continue;
                }
                self.copy(&entry.path(), &format!("{rel}/{name}"), depth + 1)?;
            }
            return Ok(());
        }
        if !meta.is_file() {
            return Ok(());
        }
        let dst = self.home_out.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let ext = src.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let rewritten = if REWRITE_EXTENSIONS.contains(&ext.as_str()) && meta.len() <= REWRITE_MAX_BYTES {
            fs::read_to_string(src).ok().map(|text| rewrite_paths(&text, self.map))
        } else {
            None
        };
        match rewritten {
            Some(text) => {
                if fs::read_to_string(&dst).ok().as_deref() != Some(text.as_str()) {
                    fs::write(&dst, text).with_context(|| format!("writing {}", dst.display()))?;
                    self.copied += 1;
                }
            }
            None => {
                let fresh = fs::metadata(&dst).is_ok_and(|d| {
                    d.len() == meta.len() && matches!((d.modified(), meta.modified()), (Ok(d), Ok(s)) if d >= s)
                });
                if !fresh {
                    fs::copy(src, &dst).with_context(|| format!("copying {}", src.display()))?;
                    self.copied += 1;
                }
            }
        }
        self.manifest.insert(rel.to_string());
        Ok(())
    }
}

fn prune(dir: &Path, root: &Path, keep: &BTreeSet<String>) -> Result<bool> {
    let mut empty = true;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if prune(&path, root, keep)? {
                let _ = fs::remove_dir(&path);
            } else {
                empty = false;
            }
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        if keep.contains(&rel) {
            empty = false;
        } else {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(empty)
}

pub struct Staged {
    /// Files that make up the sandbox copy of the host configuration.
    pub total: usize,
    /// Files written or refreshed by this run.
    pub updated: usize,
}

/// A merged settings file to place at `rel` (relative to the sandbox home).
pub struct SettingsFile {
    pub rel: String,
    pub content: String,
    /// Host `env` entries left out because their names look like credentials.
    pub withheld_env: Vec<String>,
}

/// Builds `<sync_dir>/home` (a mirror of what goes into the sandbox home) and
/// `<sync_dir>/manifest` from the host configuration.
pub fn stage(
    profile: &Profile,
    host_home: &Path,
    sandbox_home: &str,
    sync_dir: &Path,
    settings: Option<SettingsFile>,
) -> Result<Staged> {
    let home_out = sync_dir.join("home");
    fs::create_dir_all(&home_out).with_context(|| format!("creating {}", home_out.display()))?;
    let map = path_map(profile, host_home, sandbox_home);
    let mut stager =
        Stager { exclude: &profile.exclude, map: &map, home_out: &home_out, manifest: BTreeSet::new(), copied: 0 };

    if let (Some(host_dir), Some(sandbox_dir)) = (host_config_dir(profile, host_home), &profile.sandbox_config_dir) {
        for entry in &profile.copy {
            if profile.settings_files.contains(entry) || profile.exclude.contains(entry) {
                continue;
            }
            stager.copy(&host_dir.join(entry), &rel_join(sandbox_dir, entry), 0)?;
        }
    }
    for pair in &profile.extra_copy {
        stager.copy(&expand_tilde(&pair.from, host_home), pair.to.trim_matches('/'), 0)?;
    }
    if let Some(settings) = settings {
        let dst = home_out.join(&settings.rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::read_to_string(&dst).ok().as_deref() != Some(settings.content.as_str()) {
            fs::write(&dst, &settings.content).with_context(|| format!("writing {}", dst.display()))?;
            stager.copied += 1;
        }
        stager.manifest.insert(settings.rel);
    }

    let Stager { manifest, copied, .. } = stager;
    prune(&home_out, &home_out, &manifest)?;
    let mut listing = manifest.iter().cloned().collect::<Vec<_>>().join("\n");
    listing.push('\n');
    fs::write(sync_dir.join("manifest"), listing)?;
    Ok(Staged { total: manifest.len(), updated: copied })
}

/// Works out the merged settings file for a sandbox.
///
/// `base_dir` caches the settings the sandbox was created with, so repeated
/// launches merge onto pristine sandbox defaults instead of onto earlier merges.
/// `read_sandbox` reads a file (path relative to the sandbox home) from the sandbox.
pub fn plan_settings(
    profile: &Profile,
    host_home: &Path,
    sandbox_home: &str,
    base_dir: &Path,
    read_sandbox: &dyn Fn(&str) -> Result<Option<String>>,
) -> Result<Option<SettingsFile>> {
    let Some(sandbox_dir) = &profile.sandbox_config_dir else { return Ok(None) };
    if profile.settings_files.is_empty() {
        return Ok(None);
    }
    let host_file = host_config_dir(profile, host_home)
        .and_then(|dir| profile.settings_files.iter().map(|f| (f, dir.join(f))).find(|(_, p)| p.is_file()));

    let name_cache = base_dir.join("settings.name");
    let content_cache = base_dir.join("settings.base");
    let (name, base) = match (fs::read_to_string(&name_cache), fs::read_to_string(&content_cache)) {
        (Ok(name), Ok(base)) => (name.trim().to_string(), base),
        _ => {
            let mut found = None;
            for candidate in &profile.settings_files {
                if let Some(text) = read_sandbox(&rel_join(sandbox_dir, candidate))? {
                    found = Some((candidate.clone(), text));
                    break;
                }
            }
            let (name, base) = found.unwrap_or_else(|| {
                let name = host_file.as_ref().map(|(f, _)| (*f).clone());
                (name.unwrap_or_else(|| profile.settings_files.last().unwrap().clone()), String::new())
            });
            fs::create_dir_all(base_dir)?;
            fs::write(&name_cache, &name)?;
            fs::write(&content_cache, &base)?;
            (name, base)
        }
    };

    let map = path_map(profile, host_home, sandbox_home);
    let host_text = match &host_file {
        Some((_, path)) => {
            let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            Some(rewrite_paths(&text, &map))
        }
        None => None,
    };
    let Merged { content, withheld_env } = merge_settings(&base, host_text.as_deref(), profile)?;
    Ok(Some(SettingsFile { rel: rel_join(sandbox_dir, &name), content, withheld_env }))
}

/// Script run inside the sandbox to apply the staged configuration: removes
/// files that disappeared from the host since the last sync, then copies the rest.
pub fn apply_script(sandbox_home: &str) -> String {
    format!(
        r#"#!/bin/sh
# Generated by sand. Applies the host configuration staged in ~/{RUNTIME_DIR_NAME}/sync.
set -u
SRC="{sandbox_home}/{RUNTIME_DIR_NAME}/sync"
DEST="{sandbox_home}"
STATE="$DEST/.local/state/sand"
[ -f "$SRC/manifest" ] || {{ echo "sand: nothing staged in $SRC" >&2; exit 1; }}
mkdir -p "$STATE" || exit 1
if [ -f "$STATE/manifest" ]; then
  sort "$STATE/manifest" > "$STATE/old.sorted"
  sort "$SRC/manifest" > "$STATE/new.sorted"
  comm -23 "$STATE/old.sorted" "$STATE/new.sorted" | while IFS= read -r stale; do
    [ -n "$stale" ] && rm -f -- "$DEST/$stale"
  done
  rm -f "$STATE/old.sorted" "$STATE/new.sorted"
fi
if [ -d "$SRC/home" ]; then
  cp -RL "$SRC/home/." "$DEST/" || exit 1
fi
cp "$SRC/manifest" "$STATE/manifest"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    fn profile(name: &str) -> Profile {
        Profile::resolve(name, &Config::default()).unwrap()
    }

    #[test]
    fn deep_merge_merges_objects_and_replaces_the_rest() {
        let mut base = json!({"a": {"x": 1, "y": [1, 2]}, "b": "keep"});
        deep_merge(&mut base, json!({"a": {"y": [3], "z": true}, "c": null}));
        assert_eq!(base, json!({"a": {"x": 1, "y": [3], "z": true}, "b": "keep", "c": null}));
    }

    #[test]
    fn claude_settings_merge_layers_and_enforce_bypass() {
        let base = r#"{"permissions": {"defaultMode": "bypassPermissions"}, "model": "opus", "themeId": 1}"#;
        let host = r#"{
            // comments and trailing commas are fine
            "model": "sonnet",
            "apiKeyHelper": "/usr/local/bin/key",
            "permissions": {"defaultMode": "plan", "allow": ["Bash(ls:*)"]},
            "statusLine": {"type": "command", "command": "~/mine.sh"},
        }"#;
        let merged: Value =
            serde_json::from_str(&merge_settings(base, Some(host), &profile("claude")).unwrap().content).unwrap();
        assert_eq!(merged["model"], "sonnet");
        assert_eq!(merged["themeId"], 1);
        assert_eq!(merged["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(merged["permissions"]["allow"][0], "Bash(ls:*)");
        assert_eq!(merged["statusLine"]["command"], "sh /home/agent/.sand/statusline.sh");
        assert!(merged.get("apiKeyHelper").is_none());
    }

    #[test]
    fn codex_settings_merge_as_toml() {
        let base = "model = \"gpt-5\"\n[mcp_servers.gateway]\nurl = \"http://mcp-gateway.docker.internal/mcp\"\n";
        let host = "approval_policy = \"on-request\"\nmodel = \"o3\"\n[mcp_servers.local]\ncommand = \"npx\"\n[tui]\nnotifications = true\n";
        let text = merge_settings(base, Some(host), &profile("codex")).unwrap().content;
        let merged: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(merged["model"].as_str(), Some("o3"));
        assert_eq!(merged["approval_policy"].as_str(), Some("never"));
        assert_eq!(merged["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert_eq!(merged["tui"]["notifications"].as_bool(), Some(true));
        let servers = merged["mcp_servers"].as_table().unwrap();
        assert!(servers.contains_key("gateway") && !servers.contains_key("local"), "{text}");
    }

    #[test]
    fn credentials_in_the_host_env_block_are_withheld() {
        let host = r#"{"env": {"ANTHROPIC_API_KEY": "sk-secret", "GITHUB_TOKEN": "ghp", "ANTHROPIC_BASE_URL": "https://proxy", "DEBUG": "1"}}"#;
        let merged = merge_settings("{}", Some(host), &profile("claude")).unwrap();
        assert_eq!(merged.withheld_env, vec!["ANTHROPIC_API_KEY", "GITHUB_TOKEN"]);
        assert!(!merged.content.contains("sk-secret") && !merged.content.contains("ghp"));
        let value: Value = serde_json::from_str(&merged.content).unwrap();
        assert_eq!(value["env"], json!({"ANTHROPIC_BASE_URL": "https://proxy", "DEBUG": "1"}));
    }

    #[test]
    fn settings_merge_without_host_or_base() {
        let merged: Value =
            serde_json::from_str(&merge_settings("", None, &profile("opencode")).unwrap().content).unwrap();
        assert_eq!(merged, json!({"permission": "allow"}));
        assert!(merge_settings("{ nope", None, &profile("claude")).is_err());
    }

    #[test]
    fn path_rewriting_only_touches_copied_config() {
        let map: PathMap = vec![("/Users/me/.claude".into(), "/home/agent/.claude".into())];
        let text = r#"{"installPath": "/Users/me/.claude/plugins/cache/x", "cwd": "/Users/me/dev/proj", "other": "/Users/me/.claude-old/x"}"#;
        assert_eq!(
            rewrite_paths(text, &map),
            r#"{"installPath": "/home/agent/.claude/plugins/cache/x", "cwd": "/Users/me/dev/proj", "other": "/Users/me/.claude-old/x"}"#
        );
        let win: PathMap = vec![("C:\\Users\\me\\.claude".into(), "/home/agent/.claude".into())];
        assert_eq!(
            rewrite_paths(r#"{"p": "C:\\Users\\me\\.claude\\plugins\\x", "q": "C:/Users/me/.claude/y"}"#, &win),
            r#"{"p": "/home/agent/.claude/plugins/x", "q": "/home/agent/.claude/y"}"#
        );
    }

    #[test]
    fn staging_copies_config_but_never_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cfg = home.join(".claude");
        fs::create_dir_all(cfg.join("commands/review")).unwrap();
        fs::create_dir_all(cfg.join("plugins")).unwrap();
        fs::create_dir_all(cfg.join("projects/x")).unwrap();
        fs::write(cfg.join("CLAUDE.md"), "be nice").unwrap();
        fs::write(cfg.join("commands/review/go.md"), "command").unwrap();
        fs::write(cfg.join("commands/review/auth.json"), "secret").unwrap();
        fs::write(cfg.join(".credentials.json"), "secret").unwrap();
        fs::write(cfg.join("projects/x/log.jsonl"), "history").unwrap();
        fs::write(cfg.join("settings.json"), "{}").unwrap();
        // Skills are shared by sbx itself (`sbx skills import`), not copied.
        fs::create_dir_all(cfg.join("skills/pdf")).unwrap();
        fs::write(cfg.join("skills/pdf/SKILL.md"), "skill").unwrap();
        let installed = format!(r#"{{"path": "{}/plugins/p1"}}"#, cfg.display());
        fs::write(cfg.join("plugins/installed_plugins.json"), installed).unwrap();

        let mut p = profile("claude");
        p.host_config_dir = Some(cfg.to_string_lossy().into_owned());
        let sync = tmp.path().join("sync");
        let settings = SettingsFile {
            rel: ".claude/settings.json".into(),
            content: "{\"merged\":true}\n".into(),
            withheld_env: vec![],
        };
        let staged = stage(&p, &home, "/home/agent", &sync, Some(settings)).unwrap();
        assert_eq!(staged.total, 4);
        assert_eq!(staged.updated, 4);

        let out = sync.join("home/.claude");
        assert_eq!(fs::read_to_string(out.join("CLAUDE.md")).unwrap(), "be nice");
        assert_eq!(fs::read_to_string(out.join("settings.json")).unwrap(), "{\"merged\":true}\n");
        assert_eq!(
            fs::read_to_string(out.join("plugins/installed_plugins.json")).unwrap(),
            r#"{"path": "/home/agent/.claude/plugins/p1"}"#
        );
        assert!(!out.join(".credentials.json").exists());
        assert!(!out.join("commands/review/auth.json").exists());
        assert!(!out.join("projects").exists());
        assert!(!out.join("skills").exists());
        let manifest = fs::read_to_string(sync.join("manifest")).unwrap();
        assert_eq!(
            manifest,
            ".claude/CLAUDE.md\n.claude/commands/review/go.md\n.claude/plugins/installed_plugins.json\n.claude/settings.json\n"
        );

        // A second run copies nothing new and drops files that vanished on the host.
        fs::remove_dir_all(cfg.join("commands")).unwrap();
        let settings = SettingsFile {
            rel: ".claude/settings.json".into(),
            content: "{\"merged\":true}\n".into(),
            withheld_env: vec![],
        };
        let staged = stage(&p, &home, "/home/agent", &sync, Some(settings)).unwrap();
        assert_eq!((staged.total, staged.updated), (3, 0));
        assert!(!out.join("commands").exists());
    }

    #[test]
    fn settings_plan_caches_the_sandbox_base() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/settings.json"), r#"{"model": "sonnet"}"#).unwrap();
        let mut p = profile("claude");
        p.host_config_dir = Some(home.join(".claude").to_string_lossy().into_owned());
        let base_dir = tmp.path().join("base");

        let first = |rel: &str| -> Result<Option<String>> {
            assert_eq!(rel, ".claude/settings.json");
            Ok(Some(r#"{"themeId": 1}"#.to_string()))
        };
        let plan = plan_settings(&p, &home, "/home/agent", &base_dir, &first).unwrap().unwrap();
        assert_eq!(plan.rel, ".claude/settings.json");
        let merged: Value = serde_json::from_str(&plan.content).unwrap();
        assert_eq!((&merged["themeId"], &merged["model"]), (&json!(1), &json!("sonnet")));

        // Later launches must not read the (already merged) sandbox file again.
        let never = |_: &str| -> Result<Option<String>> { panic!("sandbox base should be cached") };
        fs::write(home.join(".claude/settings.json"), r#"{}"#).unwrap();
        let plan = plan_settings(&p, &home, "/home/agent", &base_dir, &never).unwrap().unwrap();
        let merged: Value = serde_json::from_str(&plan.content).unwrap();
        assert_eq!(merged["themeId"], 1);
        assert!(merged.get("model").is_none(), "host key removed on the host must disappear");
    }
}
