//! Custom templates: images with tooling pre-installed, used through `sbx template`.
//!
//! A template is either defined in config.toml (`image = …` or `dockerfile = …`)
//! or a snapshot of a sandbox taken with `sand template save`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::args;
use crate::config::{Config, TemplateDef};
use crate::log;
use crate::paths::{Dirs, expand_tilde};
use crate::sbx::{Sbx, run_host};
use crate::state;

/// Templates that live in the sandbox runtime's local image store.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    templates: BTreeMap<String, Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    tag: String,
    /// "build" (from a Dockerfile) or "snapshot" (from a sandbox).
    source: String,
    updated_at: u64,
}

fn load_registry(dirs: &Dirs) -> Result<Registry> {
    let file = dirs.templates_file();
    match fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

fn save_registry(dirs: &Dirs, registry: &Registry) -> Result<()> {
    let file = dirs.templates_file();
    fs::write(&file, serde_json::to_string_pretty(registry)?).with_context(|| format!("writing {}", file.display()))
}

fn local_tag(name: &str) -> String {
    format!("sand-template-{}:latest", state::sanitize_name(name))
}

/// What to pass to `sbx create`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub image: String,
    pub pull: Pull,
}

/// When the sandbox runtime pulls the image from its registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pull {
    /// Left to sbx: pulled when it is not in the image store yet.
    Missing,
    /// Local images must not be pulled from a registry.
    Never,
    /// Moving tags (nightly images) are pulled every time a sandbox is created.
    Always,
}

impl Resolved {
    /// The image a harness is rebuilt into every night.
    pub fn nightly(image: String) -> Self {
        Self { image, pull: Pull::Always }
    }

    pub fn create_args(&self) -> Vec<std::ffi::OsString> {
        let mut out = args!["--template", &self.image];
        match self.pull {
            Pull::Missing => {}
            Pull::Never => out.extend(args!["--pull", "never"]),
            Pull::Always => out.extend(args!["--pull", "always"]),
        }
        out
    }
}

/// Resolves a template name (or raw image reference) to an image, building
/// Dockerfile templates that have not been built yet.
pub fn resolve(sbx: &Sbx, dirs: &Dirs, config: &Config, name: &str) -> Result<Resolved> {
    let registry = load_registry(dirs)?;
    if let Some(def) = config.templates.get(name) {
        if let Some(image) = &def.image {
            if def.dockerfile.is_some() {
                bail!("[templates.{name}] sets both `image` and `dockerfile`; use one");
            }
            return Ok(Resolved { image: image.clone(), pull: Pull::Missing });
        }
        if !registry.templates.contains_key(name) {
            log::info(format!("template '{name}' has not been built yet"));
            build(sbx, dirs, config, name)?;
        }
        return Ok(Resolved { image: local_tag(name), pull: Pull::Never });
    }
    if let Some(entry) = registry.templates.get(name) {
        return Ok(Resolved { image: entry.tag.clone(), pull: Pull::Never });
    }
    // Anything else is taken as an image reference for sbx to pull.
    Ok(Resolved { image: name.to_string(), pull: Pull::Missing })
}

fn dockerfile_of(dirs: &Dirs, name: &str, def: &TemplateDef) -> Result<PathBuf> {
    let raw = def
        .dockerfile
        .as_deref()
        .ok_or_else(|| anyhow!("[templates.{name}] has no `dockerfile` to build (it uses a pre-built image)"))?;
    let path = expand_tilde(raw, &dirs.home);
    let base = dirs.config_file.parent().map(PathBuf::from).unwrap_or_default();
    let path = if path.is_absolute() { path } else { base.join(path) };
    if !path.is_file() {
        bail!("Dockerfile for template '{name}' not found at {}", path.display());
    }
    Ok(path)
}

/// `sand template build <name>`: docker build → docker save → sbx template load.
pub fn build(sbx: &Sbx, dirs: &Dirs, config: &Config, name: &str) -> Result<()> {
    let def =
        config.templates.get(name).ok_or_else(|| anyhow!("no [templates.{name}] in {}", dirs.config_file.display()))?;
    let dockerfile = dockerfile_of(dirs, name, def)?;
    let context = match &def.context {
        Some(context) => {
            let path = expand_tilde(context, &dirs.home);
            if path.is_absolute() {
                path
            } else {
                dockerfile.parent().map(PathBuf::from).unwrap_or_default().join(path)
            }
        }
        None => dockerfile.parent().map(PathBuf::from).unwrap_or_default(),
    };
    let tag = local_tag(name);

    log::step(format!("Building template '{name}' from {}", dockerfile.display()));
    let mut cmd = args!["build", "--tag", &tag, "--file", &dockerfile];
    for (key, value) in &def.build_args {
        cmd.extend(args!["--build-arg", format!("{key}={value}")]);
    }
    cmd.push(context.into_os_string());
    run_host("docker", cmd)?;

    // The sandbox runtime has its own image store, so the image is handed over as a tar.
    let tar = dirs.data.join(format!("template-{}.tar", state::sanitize_name(name)));
    log::step("Loading the image into the sandbox runtime");
    let loaded = run_host("docker", args!["image", "save", &tag, "--output", &tar])
        .and_then(|()| sbx.run(args!["template", "load", &tar]));
    let _ = fs::remove_file(&tar);
    loaded?;

    let mut registry = load_registry(dirs)?;
    registry
        .templates
        .insert(name.to_string(), Entry { tag: tag.clone(), source: "build".into(), updated_at: state::now() });
    save_registry(dirs, &registry)?;
    log::ok(format!("template '{name}' is ready ({tag}); use it with --template {name}"));
    Ok(())
}

/// `sand template save <sandbox> <name>`: snapshot a sandbox you set up by hand.
pub fn save(sbx: &Sbx, dirs: &Dirs, config: &Config, sandbox: &str, name: &str) -> Result<()> {
    if config.templates.contains_key(name) {
        bail!("'{name}' is already defined in config.toml; choose another name for the snapshot");
    }
    let tag = local_tag(name);
    log::step(format!("Saving sandbox '{sandbox}' as template '{name}'"));
    log::warn("the snapshot contains everything in the sandbox filesystem, including any credentials stored there");
    sbx.run(args!["template", "save", sandbox, &tag])?;
    let mut registry = load_registry(dirs)?;
    registry
        .templates
        .insert(name.to_string(), Entry { tag: tag.clone(), source: "snapshot".into(), updated_at: state::now() });
    save_registry(dirs, &registry)?;
    log::ok(format!("template '{name}' saved ({tag}); use it with --template {name}"));
    Ok(())
}

pub fn list(dirs: &Dirs, config: &Config) -> Result<()> {
    let registry = load_registry(dirs)?;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for (name, def) in &config.templates {
        let row = match (&def.image, &def.dockerfile) {
            (Some(image), _) => (name.clone(), "image".to_string(), image.clone()),
            (None, Some(file)) => {
                let built =
                    if registry.templates.contains_key(name) { local_tag(name) } else { "not built yet".to_string() };
                (name.clone(), "dockerfile".to_string(), format!("{file} → {built}"))
            }
            (None, None) => (name.clone(), "invalid".to_string(), "needs `image` or `dockerfile`".to_string()),
        };
        rows.push(row);
    }
    for (name, entry) in &registry.templates {
        if !config.templates.contains_key(name) {
            rows.push((name.clone(), entry.source.clone(), entry.tag.clone()));
        }
    }
    if rows.is_empty() {
        println!(
            "No templates. Define [templates.<name>] in {} or run `sand template save`.",
            dirs.config_file.display()
        );
        return Ok(());
    }
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max(4);
    println!("{:<width$}  {:<10}  SOURCE", "NAME", "KIND");
    for (name, kind, source) in rows {
        println!("{name:<width$}  {kind:<10}  {source}");
    }
    Ok(())
}

pub fn remove(sbx: &Sbx, dirs: &Dirs, name: &str) -> Result<()> {
    let mut registry = load_registry(dirs)?;
    let Some(entry) = registry.templates.remove(name) else {
        bail!("no built or saved template named '{name}' (see `sand template ls`)");
    };
    log::step(format!("Removing template '{name}'"));
    sbx.run(args!["template", "rm", &entry.tag])?;
    save_registry(dirs, &registry)?;
    log::ok(format!("removed {}", entry.tag));
    Ok(())
}
