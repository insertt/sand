//! Path translation for ACP sessions. The editor speaks in host paths and the
//! agent in sandbox paths; they are the same on Unix, but on Windows sbx
//! exposes `C:\Users\me` as `/c/Users/me`, so the protocol stream is rewritten
//! in both directions.

use serde_json::Value;

use crate::mounts::MountSet;
use crate::paths::container_path;

/// Keys whose string values are prose or file contents, never a path.
const FREE_TEXT: &[&str] = &["text", "oldText", "newText", "output"];

const FILE_URI: &str = "file://";

#[derive(Clone, Debug)]
pub struct PathMap {
    /// (host path, sandbox path) of every mount, longest sandbox path first.
    mounts: Vec<(String, String)>,
}

impl PathMap {
    /// `None` when host and sandbox paths are identical and nothing needs translating.
    pub fn new(mounts: &MountSet) -> Option<Self> {
        let mut pairs: Vec<(String, String)> = mounts
            .all()
            .map(|m| m.path.to_string_lossy().into_owned())
            .map(|host| (host.trim_end_matches(['\\', '/']).to_string(), container_path(&host)))
            .filter(|(host, sandbox)| host != sandbox)
            .collect();
        pairs.sort_by_key(|(_, sandbox)| std::cmp::Reverse(sandbox.len()));
        (!pairs.is_empty()).then_some(Self { mounts: pairs })
    }

    /// Rewrites one message from the editor for the agent.
    pub fn to_sandbox(&self, line: &str) -> String {
        translate(line, &|s| match s.strip_prefix(FILE_URI) {
            Some(path) => sandbox_path(path.trim_start_matches('/')).map(|p| format!("{FILE_URI}{p}")),
            None => sandbox_path(s),
        })
    }

    /// Rewrites one message from the agent for the editor.
    pub fn to_host(&self, line: &str) -> String {
        translate(line, &|s| match s.strip_prefix(FILE_URI) {
            Some(path) => self.host_path(path).map(|p| format!("{FILE_URI}/{}", p.replace('\\', "/"))),
            None => self.host_path(s),
        })
    }

    fn host_path(&self, path: &str) -> Option<String> {
        self.mounts.iter().find_map(|(host, sandbox)| {
            let rest = path.strip_prefix(sandbox.as_str())?;
            (rest.is_empty() || rest.starts_with('/')).then(|| format!("{host}{}", rest.replace('/', "\\")))
        })
    }
}

/// Any host path is translated, mounted or not: an unmounted one does not
/// exist in the sandbox either way, and the agent rejects the Windows form.
fn sandbox_path(path: &str) -> Option<String> {
    let translated = container_path(path);
    (translated != path).then_some(translated)
}

/// Applies `map` to every string in a JSON line that is a path on its own.
/// Anything that is not JSON passes through untouched.
fn translate(line: &str, map: &dyn Fn(&str) -> Option<String>) -> String {
    let Ok(mut message) = serde_json::from_str::<Value>(line) else { return line.to_string() };
    rewrite(&mut message, map);
    serde_json::to_string(&message).unwrap_or_else(|_| line.to_string())
}

fn rewrite(value: &mut Value, map: &dyn Fn(&str) -> Option<String>) {
    match value {
        Value::String(s) if !s.contains('\n') => {
            if let Some(translated) = map(s) {
                *s = translated;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| rewrite(item, map)),
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if !(field.is_string() && FREE_TEXT.contains(&key.as_str())) {
                    rewrite(field, map);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mounts::Mount;

    fn windows() -> PathMap {
        let mounts = MountSet::from_list(vec![Mount::rw("G:\\dev\\work\\throwaway"), Mount::rw("G:\\dev\\work")]);
        PathMap::new(&mounts).unwrap()
    }

    fn both(map: &PathMap, host: Value, sandbox: Value) {
        assert_eq!(serde_json::from_str::<Value>(&map.to_sandbox(&host.to_string())).unwrap(), sandbox);
        assert_eq!(serde_json::from_str::<Value>(&map.to_host(&sandbox.to_string())).unwrap(), host);
    }

    #[test]
    fn unix_mounts_need_no_translation() {
        assert!(PathMap::new(&MountSet::from_list(vec![Mount::rw("/home/me/proj")])).is_none());
    }

    #[test]
    fn paths_are_translated_in_both_directions() {
        let map = windows();
        both(
            &map,
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
                "params": {"cwd": "G:\\dev\\work\\throwaway", "mcpServers": []}}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
                "params": {"cwd": "/g/dev/work/throwaway", "mcpServers": []}}),
        );
        both(
            &map,
            json!({"locations": [{"path": "G:\\dev\\work\\throwaway\\src\\main.rs", "line": 3}, {"path": "G:\\dev\\work\\lib"}]}),
            json!({"locations": [{"path": "/g/dev/work/throwaway/src/main.rs", "line": 3}, {"path": "/g/dev/work/lib"}]}),
        );
        both(
            &map,
            json!({"type": "resource_link", "uri": "file:///G:/dev/work/throwaway/a%20b.rs"}),
            json!({"type": "resource_link", "uri": "file:///g/dev/work/throwaway/a%20b.rs"}),
        );
    }

    #[test]
    fn text_and_foreign_paths_are_left_alone() {
        let map = windows();
        let prompt = json!({"prompt": [{"type": "text", "text": "C:\\Windows"}], "note": "see C:\\Windows\nplease"});
        assert_eq!(map.to_sandbox(&prompt.to_string()), prompt.to_string());
        let update = json!({"path": "/usr/lib", "sibling": "/g/dev/workshop", "text": "/g/dev/work", "id": 7});
        assert_eq!(map.to_host(&update.to_string()), update.to_string());
        assert_eq!(map.to_host("not json"), "not json");
    }
}
