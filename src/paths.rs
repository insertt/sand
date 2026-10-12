//! Host-side locations used by sand (config file, data directory, per-sandbox dirs).

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Home directory of the agent user inside every Docker Sandbox template.
pub const DEFAULT_SANDBOX_HOME: &str = "/home/agent";
/// Where the per-sandbox runtime directory is mounted (read-only) inside the sandbox.
pub const RUNTIME_DIR_NAME: &str = ".sand";

#[derive(Clone, Debug)]
pub struct Dirs {
    pub home: PathBuf,
    pub data: PathBuf,
    pub config_file: PathBuf,
}

impl Dirs {
    pub fn discover() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
        let data = match env::var_os("SAND_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => {
                dirs::data_local_dir().ok_or_else(|| anyhow!("cannot determine the local data directory"))?.join("sand")
            }
        };
        let config_file = match env::var_os("SAND_CONFIG") {
            Some(file) => PathBuf::from(file),
            None => dirs::config_dir()
                .ok_or_else(|| anyhow!("cannot determine the config directory"))?
                .join("sand")
                .join("config.toml"),
        };
        std::fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
        // Mount specs handed to sbx must be absolute and free of `\\?\` prefixes.
        let data = dunce::canonicalize(&data).unwrap_or(data);
        Ok(Self { home, data, config_file })
    }

    pub fn sandbox_dir(&self, name: &str) -> PathBuf {
        self.data.join("sandboxes").join(name)
    }

    pub fn state_file(&self, name: &str) -> PathBuf {
        self.sandbox_dir(name).join("state.json")
    }

    /// Host directory mounted read-only at `~/.sand` inside the sandbox.
    pub fn runtime_dir(&self, name: &str) -> PathBuf {
        self.sandbox_dir(name).join("runtime")
    }

    pub fn history_dir(&self, name: &str) -> PathBuf {
        self.sandbox_dir(name).join("history")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.data.join("locks")
    }

    /// Workspace shared by every `--scratch` sandbox.
    pub fn scratch_dir(&self) -> PathBuf {
        self.data.join("scratch")
    }

    pub fn templates_file(&self) -> PathBuf {
        self.data.join("templates.json")
    }
}

/// Expands a leading `~` to `home`.
pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Path at which sbx exposes a host path inside the sandbox when no explicit
/// target is given: identical on Unix, `/c/Users/me` for `C:\Users\me` on Windows.
pub fn container_path(host: &str) -> String {
    let bytes = host.as_bytes();
    let is_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if !is_drive {
        return host.to_string();
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = host[3..].replace('\\', "/");
    if rest.is_empty() { format!("/{drive}") } else { format!("/{drive}/{rest}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/home/me");
        assert_eq!(expand_tilde("~", home), PathBuf::from("/home/me"));
        assert_eq!(expand_tilde("~/.claude", home), PathBuf::from("/home/me/.claude"));
        assert_eq!(expand_tilde("/abs/~", home), PathBuf::from("/abs/~"));
        assert_eq!(expand_tilde("~user/x", home), PathBuf::from("~user/x"));
    }

    #[test]
    fn container_paths() {
        assert_eq!(container_path("/home/me/proj"), "/home/me/proj");
        assert_eq!(container_path("C:\\Users\\me\\data"), "/c/Users/me/data");
        assert_eq!(container_path("D:/work"), "/d/work");
        assert_eq!(container_path("C:\\"), "/c");
    }
}
