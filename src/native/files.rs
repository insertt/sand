//! `@` mentions: files of the workspace named in the draft, completed as
//! they are typed and sent beside the text as ACP resources, as Claude Code
//! attaches the files its own `@` names.

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::args;
use crate::paths::container_path;
use crate::sbx::Sbx;

/// Paths listed at most, for a workspace that is a whole home directory.
const MAX_ENTRIES: usize = 50_000;
/// Completions shown.
const SHOWN: usize = 12;
/// Directories a walk skips besides hidden ones (git leaves out what it ignores).
const SKIPPED: &[&str] = &["node_modules", "target", "dist", "build", "__pycache__", "vendor"];
/// Files embedded in a prompt at most; larger ones go as links.
pub const MAX_EMBEDDED: u64 = 256 * 1024;

/// A file the message names: where the mention is in its text (bytes), the
/// path as written, and the file's URI on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub range: Range<usize>,
    pub path: String,
    pub uri: String,
    /// The file's contents, for an agent that takes them in the prompt.
    pub text: Option<String>,
}

/// The workspace's files and directories (with a trailing `/`), relative and
/// `/`-separated: git's view of it, tracked and untracked but not ignored,
/// or a walk outside a repository.
pub fn list(dir: &Path) -> Vec<String> {
    let mut files = git_files(dir).unwrap_or_else(|| {
        let mut files = Vec::new();
        walk(dir, "", &mut files);
        files
    });
    files.truncate(MAX_ENTRIES);
    let mut dirs: Vec<String> = Vec::new();
    for file in &files {
        let mut at = 0;
        while let Some(slash) = file[at..].find('/') {
            at += slash + 1;
            dirs.push(file[..at].to_string());
        }
    }
    files.extend(dirs);
    files.sort();
    files.dedup();
    files
}

fn git_files(dir: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout).split('\0').filter(|f| !f.is_empty()).map(str::to_string).collect()
    })
}

fn walk(dir: &Path, prefix: &str, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if files.len() >= MAX_ENTRIES {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if !SKIPPED.contains(&name.as_str()) {
                walk(&entry.path(), &format!("{prefix}{name}/"), files);
            }
        } else {
            files.push(format!("{prefix}{name}"));
        }
    }
}

/// The directory a mention reaching above the workspace lists, `../` and
/// what follows it up to the last `/`: `../a/` for `../a/b`.
pub fn above(query: &str) -> Option<&str> {
    query.starts_with("../").then(|| &query[..query.rfind('/').map_or(0, |i| i + 1)])
}

/// What the sandbox has in `dir` (from [`above`], taken from `cwd`, the
/// workspace in the sandbox), one level, as paths starting with `dir`. It
/// sees nothing of the host but `mounts` (host paths), so outside them only
/// a mount or a directory on the way to one is offered.
pub fn list_above(sbx: &Sbx, sandbox: &str, cwd: &str, mounts: &[PathBuf], dir: &str) -> Vec<String> {
    let target = sandbox_path(cwd, dir);
    let listed = sbx.capture(args!["exec", sandbox, "ls", "-1Ap", "--", &target]).ok().flatten().unwrap_or_default();
    let mounts: Vec<String> = mounts.iter().map(|m| container_path(&m.to_string_lossy())).collect();
    mounted(&target, listed.lines(), &mounts, dir)
}

/// `dir` taken from `cwd`, in the sandbox's `/`-separated paths.
fn sandbox_path(cwd: &str, dir: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in cwd.split('/').chain(dir.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The `names` in `target` (as `ls -p` writes them, directories with a `/`)
/// that come from the host: all of them inside a mount, else the mounts and
/// the directories leading to one. Hidden ones are left out.
fn mounted<'a>(target: &str, names: impl Iterator<Item = &'a str>, mounts: &[String], dir: &str) -> Vec<String> {
    let under = |path: &str, top: &str| {
        let top = top.trim_end_matches('/');
        path == top || path.strip_prefix(top).is_some_and(|rest| rest.starts_with('/'))
    };
    let inside = mounts.iter().any(|m| under(target, m));
    let mut found: Vec<String> = names
        .filter(|name| !name.is_empty() && !name.starts_with('.'))
        .filter(|name| {
            let path = format!("{}/{}", target.trim_end_matches('/'), name.trim_end_matches('/'));
            inside || (name.ends_with('/') && mounts.iter().any(|m| under(m, &path)))
        })
        .take(MAX_ENTRIES)
        .map(|name| format!("{dir}{name}"))
        .collect();
    found.sort();
    found
}

/// The completions of a mention reaching above the workspace: `../` for
/// `..`, else what `listed` (the entries of `dir`) holds, by name.
pub fn complete_above(dir: &str, listed: &[String], query: &str) -> Vec<String> {
    if query == ".." {
        return vec!["../".to_string()];
    }
    let Some(typed) = above(query) else { return Vec::new() };
    if typed != dir {
        return Vec::new();
    }
    let names: Vec<String> = listed.iter().filter_map(|e| e.strip_prefix(dir)).map(str::to_string).collect();
    complete(&names, &query[dir.len()..]).into_iter().map(|name| format!("{dir}{name}")).collect()
}

/// The mention the caret (in chars) ends, while it is typed: where its `@`
/// is (in chars) and what follows it.
pub fn at_caret(draft: &str, cursor: usize) -> Option<(usize, String)> {
    let chars: Vec<char> = draft.chars().collect();
    if chars.get(cursor).is_some_and(|c| !c.is_whitespace()) {
        return None;
    }
    let start = chars[..cursor.min(chars.len())].iter().rposition(|c| c.is_whitespace()).map_or(0, |i| i + 1);
    (chars.get(start) == Some(&'@')).then(|| (start, chars[start + 1..cursor].iter().collect()))
}

/// The entries completing `query`: names starting with it first, then
/// names and paths holding it, then paths holding its letters in order;
/// shorter paths first within each. With nothing typed, the top of the tree.
pub fn complete(entries: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let mut ranked: Vec<(u8, &String)> = entries
        .iter()
        .filter(|e| e.to_lowercase() != query)
        .filter_map(|entry| {
            let path = entry.to_lowercase();
            let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
            let rank = if query.is_empty() {
                path.trim_end_matches('/').matches('/').count().min(u8::MAX as usize) as u8
            } else if name.starts_with(&query) {
                0
            } else if name.contains(&query) {
                1
            } else if path.contains(&query) {
                2
            } else if in_order(&path, &query) {
                3
            } else {
                return None;
            };
            Some((rank, entry))
        })
        .collect();
    ranked.sort_by(|(a, x), (b, y)| a.cmp(b).then(x.len().cmp(&y.len())).then(x.cmp(y)));
    ranked.into_iter().take(SHOWN).map(|(_, e)| e.clone()).collect()
}

fn in_order(path: &str, query: &str) -> bool {
    let mut path = path.chars();
    query.chars().all(|q| path.any(|p| p == q))
}

/// How `path` is written after an `@`: quoted when it holds a space.
pub fn mention(path: &str) -> String {
    if path.contains(char::is_whitespace) { format!("@\"{path}\"") } else { format!("@{path}") }
}

/// The mentions in `text` naming something from `root` the sandbox sees
/// (see [`resolve`]): `@path` or
/// `@"a path"`, at the start or after a space. Trailing punctuation is the
/// sentence's when the path without it exists.
pub fn links(text: &str, root: &Path, mounts: &[PathBuf]) -> Vec<Link> {
    let mut links = Vec::new();
    let mut prev = None;
    for (i, c) in text.char_indices() {
        let starts = c == '@' && prev.is_none_or(char::is_whitespace);
        prev = Some(c);
        if !starts || links.last().is_some_and(|l: &Link| i < l.range.end) {
            continue;
        }
        let rest = &text[i + 1..];
        let candidates: Vec<(usize, &str)> = if let Some(quoted) = rest.strip_prefix('"') {
            let Some(end) = quoted.find('"') else { continue };
            vec![(end + 2, &quoted[..end])]
        } else {
            let word = &rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())];
            let trimmed = word.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'', '"']);
            vec![(word.len(), word), (trimmed.len(), trimmed)]
        };
        let found = candidates.into_iter().find(|(_, path)| {
            !path.is_empty() && Path::new(path).is_relative() && resolve(root, mounts, path).is_some()
        });
        if let Some((len, path)) = found {
            let uri = file_uri(&root.join(path.trim_end_matches('/')));
            links.push(Link { range: i..i + 1 + len, path: path.to_string(), uri, text: None });
        }
    }
    links
}

/// A `file://` URI for a host path, `/`-separated (`file:///C:/a%20b`).
pub fn file_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from(if path.starts_with('/') { "file://" } else { "file:///" });
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// `root/path` resolved, when it exists in the workspace or another of
/// `mounts`: what the sandbox sees of the host. The agent can put links in
/// the workspace that lead to any file of the host, so none is followed out.
fn resolve(root: &Path, mounts: &[PathBuf], path: &str) -> Option<PathBuf> {
    let root = dunce::canonicalize(root).ok()?;
    let resolved = dunce::canonicalize(root.join(path)).ok()?;
    let mounted = |m: &PathBuf| dunce::canonicalize(m).is_ok_and(|m| resolved.starts_with(m));
    (resolved.starts_with(&root) || mounts.iter().any(mounted)).then_some(resolved)
}

/// The text of the file `link` names, when it is a small text file [`resolve`] reaches.
pub fn contents(root: &Path, mounts: &[PathBuf], link: &Link) -> Option<String> {
    let path = resolve(root, mounts, &link.path)?;
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_EMBEDDED {
        return None;
    }
    String::from_utf8(fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn the_mention_at_the_caret_is_found() {
        assert_eq!(at_caret("see @src/ma", 11), Some((4, "src/ma".into())));
        assert_eq!(at_caret("@", 1), Some((0, String::new())));
        assert_eq!(at_caret("see @src now", 8), Some((4, "src".into())));
        assert_eq!(at_caret("see @src", 6), None, "the caret inside a word");
        assert_eq!(at_caret("me@example.com", 14), None);
        assert_eq!(at_caret("see src", 7), None);
    }

    #[test]
    fn completion_ranks_names_before_paths_before_letters() {
        let all = entries(&["Cargo.toml", "src/", "src/main.rs", "src/native/", "src/native/model.rs", "tests/mod.rs"]);
        assert_eq!(complete(&all, "mo"), ["tests/mod.rs", "src/native/model.rs"]);
        assert_eq!(complete(&all, "nat"), ["src/native/", "src/native/model.rs"]);
        assert_eq!(complete(&all, "snm"), ["src/native/model.rs"]);
        assert_eq!(complete(&all, "")[..3], ["src/", "Cargo.toml", "src/main.rs"]);
        assert!(!complete(&all, "src/").contains(&"src/".to_string()), "what is typed whole is not offered");
    }

    #[test]
    fn listing_adds_directories_and_skips_hidden_ones() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/native")).unwrap();
        fs::create_dir_all(dir.path().join(".git-not")).unwrap();
        fs::write(dir.path().join("src/native/a.rs"), "").unwrap();
        fs::write(dir.path().join(".git-not/x"), "").unwrap();
        fs::write(dir.path().join("README.md"), "").unwrap();
        assert_eq!(list(dir.path()), ["README.md", "src/", "src/native/", "src/native/a.rs"]);
    }

    #[test]
    fn mentions_of_existing_paths_become_links() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "fn a() {}").unwrap();
        fs::write(dir.path().join("my notes.md"), "").unwrap();
        let text = "fix @src/a.rs, see @\"my notes.md\" and @src/ not @nope or me@src/a.rs";
        let links = links(text, dir.path(), &[]);
        let paths: Vec<(&str, &str)> = links.iter().map(|l| (&text[l.range.clone()], l.path.as_str())).collect();
        assert_eq!(paths, [("@src/a.rs", "src/a.rs"), ("@\"my notes.md\"", "my notes.md"), ("@src/", "src/")]);
        assert!(links[1].uri.ends_with("/my%20notes.md"));
        assert!(!links[2].uri.ends_with('/'));
        assert_eq!(contents(dir.path(), &[], &links[0]).as_deref(), Some("fn a() {}"));
        assert_eq!(contents(dir.path(), &[], &links[2]), None);
    }

    #[cfg(unix)]
    #[test]
    fn mentions_never_lead_out_of_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("w"), dir.path().join("secret"));
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, "key").unwrap();
        fs::write(root.join("real.md"), "ok").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("notes.md")).unwrap();
        std::os::unix::fs::symlink(root.join("real.md"), root.join("alias.md")).unwrap();
        let links = links("see @notes.md and @alias.md", &root, &[]);
        assert_eq!(links.iter().map(|l| l.path.as_str()).collect::<Vec<_>>(), ["alias.md"]);
        assert_eq!(contents(&root, &[], &links[0]).as_deref(), Some("ok"));
        let forged = Link { range: 0..1, path: "notes.md".into(), uri: String::new(), text: None };
        assert_eq!(contents(&root, &[], &forged), None);
        // Above the workspace, only what the sandbox mounts.
        let lib = dir.path().join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("a.rs"), "lib").unwrap();
        std::os::unix::fs::symlink(&outside, lib.join("leak")).unwrap();
        let mounts = [lib.clone()];
        let links = super::links("see @../lib/a.rs and @../secret and @../lib/leak", &root, &mounts);
        assert_eq!(links.iter().map(|l| l.path.as_str()).collect::<Vec<_>>(), ["../lib/a.rs"]);
        assert_eq!(contents(&root, &mounts, &links[0]).as_deref(), Some("lib"));
        assert!(super::links("see @../lib/a.rs", &root, &[]).is_empty(), "not mounted, not there");
    }

    #[test]
    fn mentions_above_the_workspace_offer_what_the_sandbox_mounts() {
        assert_eq!(above("../"), Some("../"));
        assert_eq!(above("../../li"), Some("../../"));
        assert_eq!(above("../lib/sr"), Some("../lib/"));
        assert_eq!((above(".."), above("src/"), above("a/../")), (None, None, None));
        assert_eq!(sandbox_path("/h/me/w", "../"), "/h/me");
        assert_eq!(sandbox_path("/h/me/w", "../../../../"), "/");
        assert_eq!(sandbox_path("/h/me/w", "../lib/src/"), "/h/me/lib/src");

        let mounts = ["/h/me/w".to_string(), "/h/me/lib".to_string(), "/h/other/docs".to_string()];
        let ls = |text: &'static str| text.lines();
        // Beside the workspace, sbx's own files and anything not mounted are left out.
        let listed = mounted("/h/me", ls("CLAUDE.md\nkits/\nlib/\nlibrary/\nw/\n.cache/"), &mounts, "../");
        assert_eq!(listed, ["../lib/", "../w/"]);
        // A directory on the way to a mount, then all of a mount.
        assert_eq!(mounted("/h", ls("me/\nother/\nshared/"), &mounts, "../../"), ["../../me/", "../../other/"]);
        assert_eq!(mounted("/h/me/lib", ls("a.rs\nsrc/\n.git/"), &mounts, "../lib/"), ["../lib/a.rs", "../lib/src/"]);
        assert_eq!(mounted("/", ls("bin/\nh/\netc/"), &mounts, "../../../"), ["../../../h/"]);
        assert!(mounted("/h/me", ls("lib/"), &mounts[..1], "../").is_empty(), "nothing else mounted");

        let listed = ["../lib/".to_string(), "../w/".to_string()];
        assert_eq!(complete_above("", &[], ".."), ["../"]);
        assert_eq!(complete_above("../", &listed, "../"), ["../w/", "../lib/"]);
        assert_eq!(complete_above("../", &listed, "../l"), ["../lib/"]);
        assert!(complete_above("../", &listed, "../lib/s").is_empty(), "not listed yet");
    }

    #[test]
    fn uris_are_slashed_and_escaped() {
        assert_eq!(file_uri(Path::new("/home/me/a b#.rs")), "file:///home/me/a%20b%23.rs");
        assert_eq!(file_uri(Path::new("G:\\dev\\a.rs")), "file:///G:/dev/a.rs");
    }
}
