//! The main flow: find or create the sandbox, reconcile mounts and MCP servers,
//! copy the host configuration in, and hand the terminal to the agent.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use console::Term;
use dialoguer::{MultiSelect, Select};

use crate::acp;
use crate::args;
use crate::cli::{ConflictChoice, MountArgs, RunArgs};
use crate::config::{Config, StatuslineKind};
use crate::harness::{self, Profile};
use crate::lock;
use crate::log;
use crate::mcp;
use crate::mounts::{self, Mode, Mount, MountSet, Plan, Strategy};
use crate::native;
use crate::network::{self, Bridge};
use crate::paths::{Dirs, POLICY_DIR_NAME, REQUESTS_DIR_NAME, RUNTIME_DIR_NAME, container_path};
use crate::registry;
use crate::runtime::{self, ApiState, Refresher};
use crate::sbx::{KeepAlive, Sbx, shell_quote};
use crate::state::{self, InternalMount, Kind, SandboxState};
use crate::sync;
use crate::template;
use crate::vertex;

/// Set while the agent owns the terminal: Ctrl-C then belongs to the agent and
/// sand must survive it to clean up afterwards.
pub static IN_SESSION: AtomicBool = AtomicBool::new(false);

const READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Splits `[HARNESS] [MOUNT...]` into the harness (if one was named) and mount specs.
pub fn split_targets(targets: &[String], config: &Config, cwd: &Path, home: &Path) -> (Option<String>, Vec<String>) {
    let Some(first) = targets.first() else { return (None, Vec::new()) };
    let (raw, _) = mounts::split_mode(first);
    let pathlike = raw.starts_with(['.', '~', '/', '\\']) || raw.contains(['/', '\\']);
    let exists = cwd.join(crate::paths::expand_tilde(raw, home)).exists();
    let is_harness = !pathlike && (harness::is_known(first, config) || !exists);
    if is_harness { (Some(first.clone()), targets[1..].to_vec()) } else { (None, targets.to_vec()) }
}

fn validate_name(name: &str) -> Result<()> {
    let valid = name.len() >= 2
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && name != "default";
    if !valid {
        bail!(
            "invalid sandbox name '{name}': use at least two letters, numbers, hyphens or periods, starting with a letter or number"
        );
    }
    Ok(())
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    format!("{:06x}", (nanos ^ (u64::from(std::process::id()) << 12)) & 0xff_ffff)
}

/// What the command line asks for, before looking at any existing sandbox.
struct Request {
    profile: Profile,
    name: String,
    kind: Kind,
    mounts: MountSet,
    /// Whether mounts were spelled out (as opposed to defaulting to the current directory).
    explicit_mounts: bool,
    mcp: Vec<String>,
    template: Option<String>,
}

fn build_request(
    args: &RunArgs,
    dirs: &Dirs,
    config: &Config,
    harness: Option<String>,
    specs: Vec<String>,
) -> Result<Request> {
    let cwd = env::current_dir().context("determining the current directory")?;
    let harness = harness.unwrap_or_else(|| config.default_harness().to_string());
    let mut profile = Profile::resolve(&harness, config)?;
    let explicit_mounts = !specs.is_empty();
    let parse =
        |specs: &[String]| -> Result<Vec<Mount>> { specs.iter().map(|s| Mount::parse(s, &cwd, &dirs.home)).collect() };

    let (kind, mounts, default_name) = if args.dedicated_scratch {
        let mounts = MountSet { primary: None, extras: MountSet::from_list(parse(&specs)?).all().cloned().collect() };
        (Kind::Dedicated, mounts, format!("scratch-{}-{}", state::sanitize_name(&harness), unique_suffix()))
    } else if args.scratch {
        let scratch = dirs.scratch_dir();
        fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
        let scratch = dunce::canonicalize(&scratch).unwrap_or(scratch);
        let mut list = vec![Mount::rw(scratch)];
        list.extend(parse(&specs)?);
        (Kind::Scratch, MountSet::from_list(list), state::sanitize_name(&format!("scratch-{harness}")))
    } else {
        let specs = if specs.is_empty() { vec![".".to_string()] } else { specs };
        let mounts = MountSet::from_list(parse(&specs)?);
        let primary = mounts.primary.as_ref().expect("at least one mount").path.clone();
        let name = state::default_name(dirs, &state::sanitize_name(&harness), &primary)?;
        (Kind::Project, mounts, name)
    };
    let name = match &args.name {
        Some(name) => {
            validate_name(name)?;
            name.clone()
        }
        None => default_name,
    };
    profile.wire_vertex(&name);
    Ok(Request {
        template: args.template.clone().or_else(|| profile.template.clone()),
        mcp: mcp::normalize(&args.mcp)?,
        profile,
        name,
        kind,
        mounts,
        explicit_mounts,
    })
}

enum Decision {
    Create,
    /// Use the existing sandbox, applying `plan` and loading `load_mcp`.
    Reuse {
        plan: Plan,
        load_mcp: Vec<String>,
    },
    Abort,
}

fn ask(
    question: &str,
    items: &[(ConflictChoice, String)],
    preset: Option<ConflictChoice>,
    acp: bool,
) -> Result<ConflictChoice> {
    if let Some(choice) = preset {
        log::info(format!("--on-conflict {choice:?}: not asking").to_lowercase());
        return Ok(choice);
    }
    if acp {
        log::info("ACP mode cannot ask; merging (existing mounts are kept, missing ones are added)");
        return Ok(ConflictChoice::Merge);
    }
    let labels: Vec<&str> = items.iter().map(|(_, label)| label.as_str()).collect();
    if !native::active() && (!std::io::stdin().is_terminal() || !std::io::stderr().is_terminal()) {
        bail!(
            "{question}\nThis needs a decision but there is no terminal to ask on; pass --on-conflict replace|merge|abort."
        );
    }
    Ok(select(question, &labels)?.map_or(ConflictChoice::Abort, |i| items[i].0))
}

/// Asks the person to pick one of `labels`, on the native surface when one
/// shows the session, else on the terminal; `None` when they abort.
fn select(question: &str, labels: &[&str]) -> Result<Option<usize>> {
    if let Some(answer) = native::ask(question, labels) {
        return Ok(answer);
    }
    Ok(Select::new().with_prompt(question).items(labels).default(0).interact_on_opt(&Term::stderr())?)
}

fn show_set(title: &str, set: &MountSet) {
    log::info(format!("{title}:"));
    for line in set.describe() {
        log::info(format!("  {line}"));
    }
}

/// Compares an existing sand-managed sandbox with the request and decides what to do.
fn reconcile(req: &Request, current: &SandboxState, args: &RunArgs) -> Result<Decision> {
    if current.harness != req.profile.name {
        bail!(
            "sandbox '{}' was created for harness '{}', not '{}'. Use --recreate to rebuild it or --name for a separate sandbox.",
            req.name,
            current.harness,
            req.profile.name
        );
    }
    let new_mcp: Vec<String> = req.mcp.iter().filter(|m| !current.mcp.contains(m)).cloned().collect();
    let dropped_mcp: Vec<&String> = current.mcp.iter().filter(|m| !req.mcp.contains(m)).collect();
    let mounts_differ = !current.mounts.same_as(&req.mounts);
    let template_differs = args.template.is_some() && current.template != args.template;

    if !mounts_differ && dropped_mcp.is_empty() && !template_differs {
        log::ok("existing sandbox matches the requested setup");
        return Ok(Decision::Reuse {
            plan: mounts::plan(&current.mounts, &req.mounts, Strategy::Merge),
            load_mcp: new_mcp,
        });
    }

    log::step(format!("Sandbox '{}' exists with a different setup", req.name));
    if mounts_differ {
        show_set("current mounts", &current.mounts);
        show_set("requested mounts", &req.mounts);
    }
    if !dropped_mcp.is_empty() || !new_mcp.is_empty() {
        let show = |list: &[String]| if list.is_empty() { "none".to_string() } else { list.join(", ") };
        log::info(format!("MCP servers: current [{}], requested [{}]", show(&current.mcp), show(&req.mcp)));
    }
    if template_differs {
        let show = |t: &Option<String>| t.clone().unwrap_or_else(|| "default".into());
        log::info(format!("template: current {}, requested {}", show(&current.template), show(&args.template)));
    }

    let replace = mounts::plan(&current.mounts, &req.mounts, Strategy::Replace);
    let mut recreate_reasons: Vec<String> = replace.recreate.iter().cloned().collect();
    if !dropped_mcp.is_empty() {
        let names = dropped_mcp.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
        recreate_reasons.push(format!("sbx cannot detach MCP servers from an existing sandbox ({names})"));
    }
    if template_differs {
        recreate_reasons.push("the template is fixed when a sandbox is created".to_string());
    }
    for reason in &recreate_reasons {
        log::info(format!("note: matching the request exactly needs a recreate because {reason}"));
    }

    let replace_label = if recreate_reasons.is_empty() {
        "Replace — change the sandbox to exactly what this command asks for".to_string()
    } else {
        "Replace — recreate the sandbox to match this command exactly (history is preserved)".to_string()
    };
    let items = [
        (ConflictChoice::Replace, replace_label),
        (ConflictChoice::Merge, "Merge — keep what the sandbox has and add what is missing".to_string()),
        (ConflictChoice::Abort, "Abort — leave everything as it is".to_string()),
    ];
    match ask("How do you want to continue?", &items, args.on_conflict, args.acp)? {
        ConflictChoice::Abort => Ok(Decision::Abort),
        ConflictChoice::Replace if recreate_reasons.is_empty() => {
            Ok(Decision::Reuse { plan: replace, load_mcp: new_mcp })
        }
        ConflictChoice::Replace => Ok(Decision::Create),
        ConflictChoice::Merge => {
            let plan = mounts::plan(&current.mounts, &req.mounts, Strategy::Merge);
            if !dropped_mcp.is_empty() {
                log::info("keeping the MCP servers that are already attached");
            }
            if template_differs {
                log::info("keeping the sandbox's current template");
            }
            Ok(Decision::Reuse { plan, load_mcp: new_mcp })
        }
    }
}

/// Checks whether a sandbox that should run the nightly image is still on an
/// older build, and asks whether to recreate it. Returns the choice: `Replace`
/// to recreate, `Merge` to keep the sandbox, `Abort` to stop.
fn nightly_update(req: &Request, dirs: &Dirs, current: &mut SandboxState, args: &RunArgs) -> Result<ConflictChoice> {
    let keep = Ok(ConflictChoice::Merge);
    if !req.profile.nightly || req.template.is_some() {
        return keep;
    }
    let Some(image) = harness::nightly_image(&req.profile.agent) else { return keep };
    // A sandbox created from a template of its own stays on that template.
    let on_nightly = current.template.as_deref() == Some(image.as_str());
    if current.template.is_some() && !on_nightly {
        return keep;
    }
    let latest = match registry::digest(&image) {
        Ok(digest) => digest,
        Err(err) => {
            log::info(format!("could not check for a newer nightly image: {err:#}"));
            return keep;
        }
    };
    if on_nightly && current.image_digest.as_deref() == Some(latest.as_str()) {
        log::ok(format!("nightly image is up to date ({})", registry::short(&latest)));
        return keep;
    }

    let found = if on_nightly {
        let build = current.image_digest.as_deref().map(registry::short).unwrap_or("an unknown build");
        format!(
            "a newer nightly image is available: {image} is now {}, the sandbox has {build}",
            registry::short(&latest)
        )
    } else {
        format!("the sandbox was not created from the nightly image {image}")
    };
    let hint = "use --recreate to switch";
    let choice = if let Some(choice) = args.on_conflict {
        log::info(format!("{found}; --on-conflict {}: not asking", format!("{choice:?}").to_lowercase()));
        choice
    } else if current.declined_digest.as_deref() == Some(latest.as_str()) {
        log::info(format!("{found}; you chose to keep the sandbox ({hint})"));
        return keep;
    } else if args.acp || !native::active() && (!std::io::stdin().is_terminal() || !std::io::stderr().is_terminal()) {
        log::warn(format!("{found}; keeping the sandbox because there is no terminal to ask on ({hint})"));
        return keep;
    } else {
        log::step(format!("Sandbox '{}' is not on the latest nightly image", req.name));
        log::info(&found);
        let items = [
            (
                ConflictChoice::Replace,
                "Recreate — rebuild the sandbox from the new image (history is preserved, anything installed in it is lost)",
            ),
            (ConflictChoice::Merge, "Keep — stay on the current image and do not ask again for this build"),
            (ConflictChoice::Abort, "Abort — leave everything as it is"),
        ];
        let labels: Vec<&str> = items.iter().map(|(_, label)| *label).collect();
        select("How do you want to continue?", &labels)?.map_or(ConflictChoice::Abort, |i| items[i].0)
    };
    if choice == ConflictChoice::Merge {
        current.declined_digest = Some(latest);
        state::save(dirs, current)?;
    }
    Ok(choice)
}

/// A sandbox with this name exists but sand has no record of it.
fn reconcile_unmanaged(req: &Request, args: &RunArgs) -> Result<Decision> {
    log::step(format!("Sandbox '{}' exists but was not created by sand", req.name));
    log::info("sand does not know which directories it has mounted.");
    let items = [
        (ConflictChoice::Merge, "Adopt — keep the sandbox and add the requested mounts (merge)".to_string()),
        (ConflictChoice::Replace, "Recreate — remove it and create a fresh sandbox (replace)".to_string()),
        (ConflictChoice::Abort, "Abort — leave everything as it is".to_string()),
    ];
    match ask("How do you want to continue?", &items, args.on_conflict, args.acp)? {
        ConflictChoice::Abort => Ok(Decision::Abort),
        ConflictChoice::Replace => Ok(Decision::Create),
        ConflictChoice::Merge => {
            // `sbx mount` is idempotent, so mounting everything requested is safe.
            let assumed = MountSet { primary: req.mounts.primary.clone(), extras: Vec::new() };
            Ok(Decision::Reuse {
                plan: mounts::plan(&assumed, &req.mounts, Strategy::Merge),
                load_mcp: req.mcp.clone(),
            })
        }
    }
}

/// What `--recreate` can carry over to the new sandbox, beyond the command line.
#[derive(Debug, PartialEq)]
enum Keep {
    History,
    /// Extra mounts the sandbox has and the command line does not name.
    Mounts(Vec<Mount>),
    /// MCP servers the sandbox has and the command line does not name.
    Mcp(Vec<String>),
}

impl Keep {
    fn label(&self) -> String {
        match self {
            Keep::History => "conversation history".to_string(),
            Keep::Mounts(mounts) => {
                format!("mounts: {}", mounts.iter().map(Mount::to_string).collect::<Vec<_>>().join(", "))
            }
            Keep::Mcp(servers) => format!("MCP servers: {}", servers.join(", ")),
        }
    }
}

/// What there is to keep when recreating `current` for `req`; only what the
/// command line does not already ask for.
fn keepable(req: &Request, current: Option<&SandboxState>, has_history: bool) -> Vec<Keep> {
    let mut offered = Vec::new();
    if has_history {
        offered.push(Keep::History);
    }
    let Some(state) = current else { return offered };
    let mounts: Vec<Mount> =
        state.mounts.extras.iter().filter(|m| req.mounts.all().all(|r| r.path != m.path)).cloned().collect();
    if !mounts.is_empty() {
        offered.push(Keep::Mounts(mounts));
    }
    let servers: Vec<String> = state.mcp.iter().filter(|m| !req.mcp.contains(m)).cloned().collect();
    if !servers.is_empty() {
        offered.push(Keep::Mcp(servers));
    }
    offered
}

/// Asks which of `offered` to keep, all of it by default; `None` when the person aborts.
fn choose_kept(name: &str, offered: Vec<Keep>, acp: bool) -> Result<Option<Vec<Keep>>> {
    if offered.is_empty() {
        return Ok(Some(offered));
    }
    let labels: Vec<String> = offered.iter().map(Keep::label).collect();
    let picked: Vec<bool> = if native::active() {
        // The surface asks one question at a time.
        let mut picked = Vec::new();
        for label in &labels {
            match select(&format!("Recreating '{name}': keep its {label}?"), &["Keep", "Drop"])? {
                Some(answer) => picked.push(answer == 0),
                None => return Ok(None),
            }
        }
        picked
    } else if acp || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "--recreate asks what to keep of '{name}' ({}), but there is no terminal to ask on; \
             pass --force-recreate to recreate it from the command line alone",
            labels.join("; ")
        );
    } else {
        let Some(chosen) = MultiSelect::new()
            .with_prompt(format!("Recreating '{name}': keep (space toggles, enter confirms)"))
            .items(&labels)
            .defaults(&vec![true; labels.len()])
            .interact_on_opt(&Term::stderr())?
        else {
            return Ok(None);
        };
        (0..labels.len()).map(|i| chosen.contains(&i)).collect()
    };
    let mut kept = Vec::new();
    for (keep, keep_it) in offered.into_iter().zip(picked) {
        log::info(format!("{} {}", if keep_it { "keeping" } else { "dropping" }, keep.label()));
        if keep_it {
            kept.push(keep);
        }
    }
    Ok(Some(kept))
}

/// Whether there is any file below `dir`.
fn has_files(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    entries.flatten().any(|e| match e.file_type() {
        Ok(t) if t.is_dir() => has_files(&e.path()),
        Ok(_) => true,
        Err(_) => false,
    })
}

fn remove_sandbox(sbx: &Sbx, dirs: &Dirs, name: &str, keep_history: bool) -> Result<()> {
    log::step(format!("Removing sandbox '{name}'"));
    sbx.run(args!["rm", "--force", name])?;
    state::forget(dirs, name, keep_history)?;
    if keep_history {
        log::ok("removed; conversation history is preserved for the new sandbox");
    } else {
        log::ok("removed, including its conversation history");
    }
    Ok(())
}

/// The `--dedicated-scratch` sandbox of this run, while it still has to be removed.
static DISPOSABLE: Mutex<Option<Disposable>> = Mutex::new(None);

struct Disposable {
    sbx: Sbx,
    dirs: Dirs,
    name: String,
}

/// Removes the dedicated scratch sandbox when dropped, however the run ends:
/// after the session, or early with an error.
struct DisposeGuard;

impl DisposeGuard {
    /// Armed before the sandbox is created, so that a create cut short is cleaned up too.
    fn arm(sbx: &Sbx, dirs: &Dirs, name: &str) -> Self {
        let disposable = Disposable { sbx: sbx.clone(), dirs: dirs.clone(), name: name.to_string() };
        *DISPOSABLE.lock().unwrap_or_else(PoisonError::into_inner) = Some(disposable);
        Self
    }
}

impl Drop for DisposeGuard {
    fn drop(&mut self) {
        dispose();
    }
}

/// Removes the dedicated scratch sandbox of this run, if there is one. Also
/// called on Ctrl-C, where the process exits without running destructors. The
/// lock is held while removing, so that the other caller waits for it.
pub fn dispose() {
    let mut pending = DISPOSABLE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(Disposable { sbx, dirs, name }) = pending.take() else { return };
    // sbx may not have got as far as creating it.
    if !sbx.exists(&name).unwrap_or(true) {
        let _ = state::forget(&dirs, &name, false);
        return;
    }
    log::info("this was a dedicated scratch sandbox; cleaning it up");
    if let Err(err) = remove_sandbox(&sbx, &dirs, &name, false) {
        log::warn(format!("could not remove it: {err:#}. Remove it with `sbx rm {name}`."));
    }
}

/// Registers the Vertex AI access token as a custom secret of the sandbox. sbx
/// runs the token command on the host and keeps the token fresh; the sandbox
/// only ever holds the placeholder.
/// Registers the token secret unless `registered` shows sbx already has this
/// one; returns what to record as registered.
fn register_vertex_secret(
    sbx: &Sbx,
    profile: &Profile,
    name: &str,
    registered: Option<&str>,
) -> Result<Option<String>> {
    let Some(vertex) = &profile.vertex else { return Ok(None) };
    let args = vertex.secret_args(name, profile.vertex_token_env.as_deref());
    let fingerprint = args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
    if registered == Some(fingerprint.as_str()) {
        log::ok("Vertex AI access token already registered with sbx");
        return Ok(Some(fingerprint));
    }
    log::step("Registering the Vertex AI access token with sbx");
    log::info(format!("project {}, region {}", vertex.project, vertex.region));
    log::info(format!(
        "token: `{}` on the host, refresh {}, sent to {}",
        vertex.token_command,
        vertex.refresh,
        vertex.hosts.join(", ")
    ));
    sbx.run(args).with_context(|| {
        format!(
            "registering the token secret (does `{}` print a token on the host? \
             `sbx secret set-custom --show-error` shows why sbx rejected it)",
            vertex.token_command
        )
    })?;
    log::ok("the sandbox proxy adds the token to Vertex AI requests; it never enters the sandbox");
    Ok(Some(fingerprint))
}

/// `NAME=placeholder` for the agent's environment when the token is read from one.
fn vertex_env(profile: &Profile, name: &str) -> Option<String> {
    let env = profile.vertex.as_ref().and(profile.vertex_token_env.as_deref())?;
    Some(format!("{env}={}", vertex::placeholder(name)))
}

fn create_sandbox(sbx: &Sbx, dirs: &Dirs, config: &Config, req: &Request) -> Result<SandboxState> {
    log::step(format!("Creating sandbox '{}' for {}", req.name, req.profile.name));
    for line in req.mounts.describe() {
        log::info(line);
    }
    let mut cmd = args!["create", "--name", &req.name];
    let mut template = req.template.clone();
    let mut image_digest = None;
    if let Some(name) = &req.template {
        let resolved = template::resolve(sbx, dirs, config, name)?;
        log::info(format!("template: {name} ({})", resolved.image));
        if req.profile.nightly {
            log::info("nightly: not used, the template decides the image");
        }
        cmd.extend(resolved.create_args());
    } else if req.profile.nightly {
        match harness::nightly_image(&req.profile.agent) {
            Some(image) => {
                image_digest = match registry::digest(&image) {
                    Ok(digest) => Some(digest),
                    Err(err) => {
                        log::warn(format!("cannot tell which build of {image} this is: {err:#}"));
                        None
                    }
                };
                let build = image_digest.as_deref().map(|d| format!(" ({})", registry::short(d))).unwrap_or_default();
                log::info(format!("nightly image: {image}{build}"));
                cmd.extend(template::Resolved::nightly(image.clone()).create_args());
                template = Some(image);
            }
            None => {
                log::warn(format!("no nightly image is known for agent '{}'; using the sbx default", req.profile.agent))
            }
        }
    }
    if !req.mcp.is_empty() {
        cmd.extend(args!["--static-mcp", req.mcp.join(",")]);
    }
    cmd.push(OsString::from(&req.profile.agent));
    // A sandbox without a primary workspace is created mountless; its extra
    // mounts are attached afterwards because the first path would become primary.
    let mut mounts = MountSet { primary: req.mounts.primary.clone(), extras: Vec::new() };
    if let Some(primary) = &req.mounts.primary {
        cmd.push(primary.create_arg());
        cmd.extend(req.mounts.extras.iter().map(Mount::create_arg));
        mounts.extras = req.mounts.extras.clone();
    }
    sbx.run_slow(cmd, "waiting for sbx (downloading a new image can take a few minutes)")?;
    let state = SandboxState {
        name: req.name.clone(),
        harness: req.profile.name.clone(),
        agent: req.profile.agent.clone(),
        kind: req.kind,
        mounts,
        mcp: req.mcp.clone(),
        template,
        image_digest,
        declined_digest: None,
        internal_mounts: Vec::new(),
        vertex_secret: None,
        created_at: state::now(),
    };
    state::save(dirs, &state)?;
    log::ok("sandbox created");
    Ok(state)
}

/// Warns about mounts that reach sand's per-sandbox directories: through them
/// the agent could read other sandboxes' history and, read-write, change it and
/// file network prompts in another sandbox's name.
fn warn_exposing<'a>(dirs: &Dirs, mounts: impl Iterator<Item = &'a Mount>) {
    let sandboxes = dirs.sandboxes_dir();
    for mount in mounts.filter(|m| mounts::exposes(&m.path, &sandboxes)) {
        let what = match mount.mode {
            Mode::Ro => "read the history of every sandbox sand manages",
            Mode::Rw => {
                "read and change the history of every sandbox sand manages and file network prompts in their name"
            }
        };
        log::warn(format!(
            "{} contains sand's per-sandbox data ({}): the agent can {what}; mount a narrower directory",
            mount.path.display(),
            sandboxes.display()
        ));
    }
}

fn apply_plan(sbx: &Sbx, dirs: &Dirs, state: &mut SandboxState, plan: Plan) -> Result<()> {
    for note in &plan.notes {
        log::warn(note);
    }
    if plan.is_noop() {
        return Ok(());
    }
    log::step("Updating mounts");
    for mount in &plan.umount {
        log::info(format!("unmounting {mount}"));
        sbx.run(args!["umount", &state.name, mount.umount_spec()])?;
        state.mounts.extras.retain(|m| m.path != mount.path);
        state::save(dirs, state)?;
    }
    for mount in &plan.mount {
        log::info(format!("mounting {mount}"));
        sbx.run(args!["mount", &state.name, mount.mount_spec()])?;
        state.mounts.extras.retain(|m| m.path != mount.path);
        state.mounts.extras.push(mount.clone());
        state::save(dirs, state)?;
    }
    state.mounts = plan.result;
    state::save(dirs, state)?;
    log::ok("mounts updated");
    Ok(())
}

/// Mounts sand's own directories: the runtime dir (status line, staged config),
/// the network-prompt bridge and, unless the sandbox is disposable, host-backed
/// history directories.
fn ensure_internal_mounts(
    sbx: &Sbx,
    dirs: &Dirs,
    config: &Config,
    profile: &Profile,
    state: &mut SandboxState,
) -> Result<()> {
    let home = config.sandbox_home();
    let runtime = dirs.runtime_dir(&state.name);
    let mut wanted = vec![(runtime, format!("{home}/{RUNTIME_DIR_NAME}"), Mode::Ro)];
    if profile.network_prompts {
        wanted.push((dirs.policy_dir(&state.name), format!("{home}/{POLICY_DIR_NAME}"), Mode::Ro));
        wanted.push((dirs.requests_dir(&state.name), format!("{home}/{REQUESTS_DIR_NAME}"), Mode::Rw));
    }
    if state.kind != Kind::Dedicated {
        for rel in &profile.history {
            let host = dirs.history_dir(&state.name).join(rel.replace('/', "_"));
            wanted.push((host, format!("{home}/{}", rel.trim_matches('/')), Mode::Rw));
        }
    }
    // sbx restores saved mounts when a sandbox starts and fails if a host
    // directory is gone, so these must exist on every launch.
    for (host, _, _) in &wanted {
        fs::create_dir_all(host).with_context(|| format!("creating {}", host.display()))?;
    }
    let missing: Vec<InternalMount> = wanted
        .into_iter()
        .map(|(host, target, mode)| InternalMount { host: host.to_string_lossy().into_owned(), target, mode })
        .filter(|m| !state.internal_mounts.contains(m))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    log::step("Mounting sand's own directories");
    let targets = missing.iter().map(|m| shell_quote(&m.target)).collect::<Vec<_>>().join(" ");
    sbx.exec_sh(&state.name, &format!("mkdir -p {targets}"))?;
    for mount in missing {
        let what = if mount.target.ends_with(POLICY_DIR_NAME) || mount.target.ends_with(REQUESTS_DIR_NAME) {
            "network prompts"
        } else if mount.mode == Mode::Ro {
            "status"
        } else {
            "history"
        };
        log::info(format!("{what}: {} → {}", mount.host, mount.target));
        let mut spec = OsString::from(&mount.host);
        spec.push(format!(":{}:{}", mount.target, mount.mode));
        sbx.run(args!["mount", &state.name, spec])?;
        state.internal_mounts.push(mount);
        state::save(dirs, state)?;
    }
    log::ok("mounted");
    Ok(())
}

/// Refreshes sbx's shared skills store from the host. sbx mounts the store
/// where the agent reads skills each time the sandbox starts.
fn import_skills(sbx: &Sbx) -> Result<()> {
    log::step("Importing host skills into sbx's shared skills store");
    if sbx.status(args!["skills", "import", "--force"])?.success() {
        log::ok("skills imported");
    } else {
        log::warn("`sbx skills import` failed; host skills are not shared with the sandbox (is sbx up to date?)");
    }
    Ok(())
}

fn sync_config(sbx: &Sbx, dirs: &Dirs, config: &Config, profile: &Profile, name: &str) -> Result<()> {
    if profile.sandbox_config_dir.is_none() {
        log::info(format!("'{}' has no configuration profile; nothing is copied from the host", profile.name));
        return Ok(());
    }
    let home = config.sandbox_home();
    let host_dir = sync::host_config_dir(profile, &dirs.home);
    match &host_dir {
        Some(dir) if dir.is_dir() => {
            log::step(format!("Copying {} configuration from {}", profile.name, dir.display()))
        }
        _ => log::step(format!("Applying sand's {} settings (no host configuration found)", profile.name)),
    }
    let runtime = dirs.runtime_dir(name);
    let read = |rel: &str| sbx.read_file(name, &format!("{home}/{rel}"));
    let settings = sync::plan_settings(profile, &dirs.home, home, &dirs.sandbox_dir(name).join("base"), &read)?;
    if let Some(settings) = &settings {
        log::info(format!(
            "settings: sandbox defaults + host settings + bypass-permissions overlay → ~/{}",
            settings.rel
        ));
        if !settings.withheld_env.is_empty() {
            log::warn(format!(
                "not copying credential-like env entries from the host settings: {} (use `sbx secret set` instead)",
                settings.withheld_env.join(", ")
            ));
        }
    }
    let generated = if profile.network_prompts { network::mod_files() } else { Vec::new() };
    let staged = sync::stage(profile, &dirs.home, home, &runtime.join("sync"), settings, &generated)?;
    fs::write(runtime.join("apply.sh"), sync::apply_script(home))?;
    log::info(format!(
        "{} file(s) staged, {} changed since the last launch; credentials are never copied",
        staged.total, staged.updated
    ));
    sbx.run(args!["exec", name, "sh", format!("{home}/{RUNTIME_DIR_NAME}/apply.sh")])?;
    log::ok("configuration applied");
    Ok(())
}

/// Variables by which a terminal makes itself known. The agent picks how to
/// raise a desktop notification (the escape sequence its terminal turns into
/// one, under the terminal's own notification permission) by them, and inside
/// the sandbox it cannot see the host terminal's.
const TERMINAL_ENV: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "ITERM_SESSION_ID",
    "KITTY_WINDOW_ID",
    "WT_SESSION",
];

/// `NAME=value` for each terminal variable `lookup` finds set. Where only
/// `TERM` names the terminal (Ghostty or kitty reached over SSH, which forwards
/// nothing else), `TERM_PROGRAM` is derived from it; `TERM` itself stays the
/// sandbox's, whose terminfo may not know the host terminal's.
fn terminal_env(lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let get = |name: &str| lookup(name).filter(|v| !v.is_empty());
    let mut pairs: Vec<String> =
        TERMINAL_ENV.iter().filter_map(|name| get(name).map(|v| format!("{name}={v}"))).collect();
    if get("TERM_PROGRAM").is_none() {
        let program = match get("TERM").as_deref() {
            Some("xterm-ghostty") => Some("ghostty"),
            Some("xterm-kitty") => Some("kitty"),
            _ => None,
        };
        pairs.extend(program.map(|p| format!("TERM_PROGRAM={p}")));
    }
    pairs
}

struct TernHandoff {
    /// Tern's executable.
    tern: String,
    /// The id of the custom agent running `sand <harness> --acp`.
    agent: String,
}

/// In a Tern pane that opens agent CLIs as ACP chats, with sand registered
/// there (`--install-acp tern`), the session opens in Tern's chat instead of
/// the agent's terminal UI. Tern starts `sand <harness> --acp` in the current
/// directory, so only a session that picks the same sandbox is handed over.
/// Questions are asked and changes the command line asks for are made here
/// first; the ACP session keeps the sandbox's mounts and MCP servers.
fn tern_handoff(
    args: &RunArgs,
    req: &Request,
    dirs: &Dirs,
    config: &Config,
    cwd: &Path,
    native: bool,
) -> Option<TernHandoff> {
    let tern = crate::tern::acp_executable(|name| env::var(name).ok())?;
    if args.acp || args.no_attach || req.profile.acp_command.is_empty() {
        return None;
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    let settings = crate::tern::settings_path(dirs).ok().and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
    let default_harness = req.profile.name == config.default_harness();
    let Some(agent) = crate::tern::find_agent(&settings, &req.profile.name, default_harness) else {
        if !native && !crate::tern::has_sand_agent(&settings) {
            log::info(format!(
                "in Tern, `sand {} --install-acp tern` opens sessions in Tern's chat instead",
                req.profile.name
            ));
        }
        return None;
    };

    let here = Mount::parse(".", cwd, &dirs.home).ok().map(|m| m.path);
    let reason = if req.kind != Kind::Project {
        Some("a scratch sandbox")
    } else if args.name.is_some() {
        Some("--name")
    } else if req.mounts.primary.as_ref().map(|m| &m.path) != here.as_ref() {
        Some("a primary workspace other than this directory")
    } else if !args.agent_args.is_empty() {
        Some("agent arguments")
    } else {
        None
    };
    if let Some(reason) = reason {
        log::info(format!(
            "opening the terminal UI: Tern's chat (agent '{agent}') starts in this directory and cannot take {reason}"
        ));
        return None;
    }
    Some(TernHandoff { tern, agent })
}

/// Runs `tern acp <agent>`, which opens the chat in this pane and starts
/// `sand <harness> --acp`; that session takes the lock itself. Called once the
/// sandbox is fully set up, which keeps it running until the chat ends; the
/// handoff lock held meanwhile tells the ACP session to skip the setup.
fn open_in_tern(
    handoff: TernHandoff,
    dirs: &Dirs,
    profile: &Profile,
    state: &SandboxState,
    lock: lock::SessionLock,
    keep_alive: KeepAlive,
) -> Result<i32> {
    let TernHandoff { tern, agent } = handoff;
    // Taken before the session lock is let go, so that the ACP session sees it.
    let handed_over = lock::hand_over(dirs, &state.name)?;
    drop(lock);
    log::step(format!("Opening {} in Tern's chat (agent '{agent}')", profile.name));
    log::command(format!("{tern} acp {agent}"));
    IN_SESSION.store(true, Ordering::SeqCst);
    let status = std::process::Command::new(&tern).args(["acp", &agent]).status();
    IN_SESSION.store(false, Ordering::SeqCst);
    drop(handed_over);
    drop(keep_alive);
    let status = status.with_context(|| format!("starting {tern}"))?;
    log::step(format!("Session ended ({status})"));
    Ok(status.code().unwrap_or(1))
}

/// Bypass-permissions flags are part of sbx's default agent command, but a
/// bare first argument replaces those defaults, so they are put back.
fn agent_args(profile: &Profile, user: &[String]) -> Vec<String> {
    match user.first() {
        None => Vec::new(),
        Some(first) if first.starts_with('-') => user.to_vec(),
        Some(_) => {
            let mut out = profile.bypass_args.clone();
            out.extend(user.iter().cloned());
            out
        }
    }
}

pub fn run(args: RunArgs, dirs: &Dirs, config: &Config) -> Result<i32> {
    let cwd = env::current_dir().context("determining the current directory")?;
    let (harness, specs) = split_targets(&args.targets, config, &cwd, &dirs.home);

    if let Some(client) = args.install_acp {
        crate::editor::install(client, dirs, harness.as_deref())?;
        return Ok(0);
    }

    let mut req = build_request(&args, dirs, config, harness, specs)?;
    warn_exposing(dirs, req.mounts.all());
    let native = native_eligible(&args, &req.profile, config);
    let handoff = tern_handoff(&args, &req, dirs, config, &cwd, native);
    let profile = &req.profile;

    // Before sbx lists the sandboxes, so that what it lists stays true.
    let description = match &req.mounts.primary {
        Some(primary) => format!("{} on {}", profile.name, primary.path.display()),
        None => format!("{} scratch session", profile.name),
    };
    let lock = lock::acquire(dirs, &req.name, &description)?;

    // Tern's chat, opened by a session that has just set this sandbox up.
    if args.acp
        && lock::handed_over(dirs, &req.name)
        && let Some(state) = state::load(dirs, &req.name)?
    {
        return attach_prepared(&args, dirs, profile, &state, lock);
    }

    let (sbx, listed) = Sbx::connect_listing(!args.acp)?;
    // From here on, a Tern pane shows setup natively; sbx no longer gets the terminal then.
    let sbx = if native && native::start(native_info(&req, dirs)) { sbx.quiet() } else { sbx };
    if args.native && !native::active() {
        log::warn("--native: this terminal does not speak the Tern Surface Protocol; opening the terminal UI");
    }
    log::step(format!("sand: {} in sandbox '{}'", profile.name, req.name));
    if profile.is_basic() {
        log::info(format!(
            "'{}' is not a harness sand has a profile for; running it without config copy, history or status line",
            profile.name
        ));
    }
    // In ACP mode nobody reads it before the agent starts.
    if !args.acp {
        log::info(format!("using {}", sbx.version()?));
    }
    log::ok("session lock acquired");

    // --- find out what exists -------------------------------------------------
    log::step("Looking for an existing sandbox");
    let mut exists = match listed {
        Some(names) => names.contains(&req.name),
        None => sbx.exists(&req.name)?,
    };
    let mut current = state::load(dirs, &req.name)?;
    if !exists && current.is_some() {
        log::info("sand has a record of this sandbox but sbx does not; it was removed outside sand (history is kept)");
        state::forget(dirs, &req.name, true)?;
        current = None;
    }
    log::info(if exists { format!("found '{}'", req.name) } else { format!("no sandbox named '{}'", req.name) });

    if req.kind == Kind::Dedicated && exists {
        bail!("a sandbox named '{}' already exists; --dedicated-scratch always creates a new one", req.name);
    }
    if args.resume && !exists {
        let hint = if args.name.is_some() {
            ""
        } else if req.explicit_mounts {
            ", or pass --name to pick another sandbox. The first directory picks the sandbox; \
             `sand mount DIR` adds a directory to the current directory's sandbox"
        } else {
            ", or pass --name to pick another sandbox"
        };
        bail!("nothing to resume: there is no sandbox named '{}'. Run without --resume to create it{hint}.", req.name);
    }
    if args.force_recreate || args.recreate {
        let kept = if args.recreate {
            let offered = keepable(&req, current.as_ref(), has_files(&dirs.history_dir(&req.name)));
            let Some(kept) = choose_kept(&req.name, offered, args.acp)? else {
                log::info("aborted; nothing was changed");
                return Ok(1);
            };
            kept
        } else {
            Vec::new()
        };
        let mut keep_history = false;
        for keep in kept {
            match keep {
                Keep::History => keep_history = true,
                Keep::Mounts(extras) => req.mounts.extras.extend(extras),
                Keep::Mcp(servers) => req.mcp.extend(servers),
            }
        }
        if exists {
            remove_sandbox(&sbx, dirs, &req.name, keep_history)?;
        } else if !keep_history {
            state::forget(dirs, &req.name, false)?;
        }
        exists = false;
        current = None;
    }

    // ACP mode cannot ask about a newer image, so it does not look for one.
    if let (true, false, false, Some(state)) = (exists, args.resume, args.acp, current.as_mut()) {
        match nightly_update(&req, dirs, state, &args)? {
            ConflictChoice::Merge => {}
            ConflictChoice::Replace => {
                // Only the image changes: the new sandbox keeps the mounts and MCP
                // servers of the old one, plus anything this command adds.
                req.mounts = mounts::plan(&state.mounts, &req.mounts, Strategy::Merge).result;
                for server in &state.mcp {
                    if !req.mcp.contains(server) {
                        req.mcp.push(server.clone());
                    }
                }
                remove_sandbox(&sbx, dirs, &req.name, true)?;
                exists = false;
                current = None;
            }
            ConflictChoice::Abort => {
                log::info("aborted; nothing was changed");
                return Ok(1);
            }
        }
    }

    mcp::ensure_registered(&sbx, config, &req.mcp)?;

    // --- decide ---------------------------------------------------------------
    let decision = if !exists {
        Decision::Create
    } else if args.resume {
        let load_mcp = match &current {
            Some(state) => req.mcp.iter().filter(|m| !state.mcp.contains(m)).cloned().collect(),
            None => req.mcp.clone(),
        };
        // Like MCP servers, mounts on the command line are added to what the sandbox has.
        // Without any, the current directory is only what picked the sandbox.
        let plan = match &current {
            Some(state) if req.explicit_mounts => mounts::plan(&state.mounts, &req.mounts, Strategy::Merge),
            Some(state) => mounts::plan(&state.mounts, &state.mounts, Strategy::Merge),
            None => {
                if req.explicit_mounts {
                    log::warn(format!(
                        "'{}' was not created by sand, so its mounts are unknown; the mounts on the command line are ignored",
                        req.name
                    ));
                }
                Plan::default()
            }
        };
        Decision::Reuse { plan, load_mcp }
    } else {
        match &current {
            Some(state) => reconcile(&req, state, &args)?,
            None => reconcile_unmanaged(&req, &args)?,
        }
    };

    // Before the sandbox is created or started, so that its gateway finds the credentials.
    let used_mcp: Vec<String> = match &decision {
        Decision::Abort => Vec::new(),
        Decision::Create => req.mcp.clone(),
        Decision::Reuse { load_mcp, .. } => current.iter().flat_map(|s| &s.mcp).chain(load_mcp).cloned().collect(),
    };
    // ACP mode has no terminal to authorize on.
    if !args.acp {
        let can_prompt = std::io::stdin().is_terminal() && std::io::stderr().is_terminal() && !native::active();
        mcp::ensure_authorized(&sbx, &used_mcp, can_prompt)?;
    }

    let mut disposal = None;
    let (mut state, plan, load_mcp) = match decision {
        Decision::Abort => {
            log::info("aborted; nothing was changed");
            return Ok(1);
        }
        Decision::Create => {
            if exists {
                remove_sandbox(&sbx, dirs, &req.name, true)?;
            }
            if req.kind == Kind::Dedicated {
                disposal = Some(DisposeGuard::arm(&sbx, dirs, &req.name));
            }
            // Before the sandbox exists, so that sbx sets the token variable in it.
            let secret = register_vertex_secret(&sbx, profile, &req.name, None)?;
            let mut state = create_sandbox(&sbx, dirs, config, &req)?;
            state.vertex_secret = secret;
            // Extra mounts of a mountless sandbox still have to be attached.
            let pending = mounts::plan(&state.mounts, &req.mounts, Strategy::Merge);
            (state, pending, Vec::new())
        }
        Decision::Reuse { plan, load_mcp } => {
            let mut state = current.unwrap_or_else(|| SandboxState {
                name: req.name.clone(),
                harness: profile.name.clone(),
                agent: profile.agent.clone(),
                kind: req.kind,
                mounts: if args.resume {
                    MountSet::default()
                } else {
                    MountSet { primary: req.mounts.primary.clone(), extras: Vec::new() }
                },
                mcp: Vec::new(),
                template: None,
                image_digest: None,
                declined_digest: None,
                internal_mounts: Vec::new(),
                vertex_secret: None,
                created_at: state::now(),
            });
            log::step(format!("Using existing sandbox '{}'", req.name));
            state.vertex_secret = register_vertex_secret(&sbx, profile, &req.name, state.vertex_secret.as_deref())?;
            (state, plan, load_mcp)
        }
    };
    state::save(dirs, &state)?;

    // --- bring the sandbox up to the requested setup ----------------------------
    // The host directories behind saved mounts must exist before sbx starts the sandbox.
    for mount in &state.internal_mounts {
        let _ = fs::create_dir_all(&mount.host);
    }
    if profile.import_skills {
        import_skills(&sbx)?;
    }
    log::step("Starting the sandbox");
    let keep_alive = sbx.keep_alive(&state.name)?;
    sbx.wait_ready(&state.name, READY_TIMEOUT)?;
    log::ok("sandbox is running");

    apply_plan(&sbx, dirs, &mut state, plan)?;
    ensure_internal_mounts(&sbx, dirs, config, profile, &mut state)?;

    if !load_mcp.is_empty() {
        log::step(format!("Attaching MCP servers: {}", load_mcp.join(", ")));
        for server in load_mcp {
            mcp::load(&sbx, &state.name, &server)?;
            state.mcp.push(server);
            state::save(dirs, &state)?;
        }
        log::ok("MCP servers attached");
    }

    // Extra mounts are working directories of Claude Code, so that `@` suggests their files.
    let mut synced = profile.clone();
    if let Some(extra) = network::working_directories(&state.mounts).filter(|_| profile.network_prompts) {
        sync::deep_merge(&mut synced.overlay, extra);
    }
    sync_config(&sbx, dirs, config, &synced, &state.name)?;

    log::step("Checking agent API and MCP servers");
    let runtime_dir = dirs.runtime_dir(&state.name);
    runtime::write_info(&runtime_dir, &state.name, profile)?;
    let mcp_status = mcp::status(&sbx, &state.mcp);
    runtime::write_mcp_status(&runtime_dir, &mcp_status)?;
    let api = if args.acp { ApiState::Skipped } else { runtime::check_api(&sbx, &state.name, profile) };
    if native::active() {
        native::update(native::Update::Mounts(state.mounts.describe()));
        native::update(native::Update::Api(api));
        native::update(native::Update::Mcp(mcp_status.clone()));
    } else {
        runtime::banner(&state.name, profile, &state.mounts, api, &mcp_status);
    }
    if args.no_attach {
        log::ok(format!("sandbox '{}' is ready; `sand {} --resume` attaches to it", state.name, profile.name));
        return Ok(0);
    }
    if profile.statusline != StatuslineKind::Claude && !args.acp && !native::active() {
        let shows = match profile.statusline {
            StatuslineKind::Codex => "status line shows model, branch, context and limits, but not the sandbox",
            _ => "cannot run a custom status line",
        };
        log::info(format!(
            "{} {shows}; run `sand status --name {}` in another terminal to see this again",
            profile.name, state.name
        ));
    }

    // --- hand over to the agent -------------------------------------------------
    let code = if native::active() {
        attach_native(&sbx, dirs, config, profile, &state, keep_alive)?
    } else if let Some(handoff) = handoff {
        return open_in_tern(handoff, dirs, profile, &state, lock, keep_alive);
    } else {
        attach(&sbx, dirs, &args, profile, &state, Some(keep_alive))?.code().unwrap_or(1)
    };
    if disposal.is_some() {
        drop(disposal);
    } else {
        log::info(format!("sandbox '{}' is kept; `sand {} --resume` attaches to it again", state.name, profile.name));
    }
    Ok(code)
}

/// Whether the session may be drawn natively in Tern, should the terminal
/// turn out to speak the Tern Surface Protocol.
fn native_eligible(args: &RunArgs, profile: &Profile, config: &Config) -> bool {
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let forced = args.forced_native();
    let configured = config.tern_native.unwrap_or(true);
    let eligible = tty && native::eligible(args.acp, args.no_attach, profile, &args.agent_args, forced, configured);
    if forced == Some(true) && !eligible {
        log::warn("--native: not drawing natively (it needs a terminal and an agent with an ACP command)");
    }
    if tty && !eligible && !args.agent_args.is_empty() && env::var("TERM_PROGRAM").is_ok_and(|t| t == "tern") {
        log::info("opening the terminal UI: Tern draws sessions natively only without agent arguments");
    }
    eligible
}

/// What the native surface shows about the session before the sandbox is set up.
fn native_info(req: &Request, dirs: &Dirs) -> native::Info {
    let profile = &req.profile;
    let (model, effort) = sync::host_model(profile, &dirs.home);
    let api_host = profile.api_check_url.as_deref().map(|url| {
        let rest = url.split("//").nth(1).unwrap_or(url);
        rest.split('/').next().unwrap_or(rest).to_string()
    });
    native::Info {
        harness: profile.name.clone(),
        product: crate::tern::product_name(&profile.name).to_string(),
        sandbox: req.name.clone(),
        workspace: req.mounts.primary.as_ref().map(|m| tilde(&m.path, &dirs.home)),
        mounts: req.mounts.describe(),
        mcp: req.mcp.iter().map(|name| mcp::McpStatus { name: name.clone(), state: mcp::McpState::Unknown }).collect(),
        api: None,
        api_host,
        vertex: profile.vertex.is_some(),
        branch: None,
        model,
        effort,
    }
}

fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Runs the chat on the native surface that showed setup: sand starts the
/// agent's ACP server and is its client. `keep_alive` is let go once it ends.
fn attach_native(
    sbx: &Sbx,
    dirs: &Dirs,
    config: &Config,
    profile: &Profile,
    state: &SandboxState,
    keep_alive: KeepAlive,
) -> Result<i32> {
    log::step(format!("Starting {}", crate::tern::product_name(&profile.name)));
    // Mounts and hosts the agent asks for, and sbx's network approvals, show in the pane.
    let hooks = native::bridge_hooks();
    let spec = native::ChatSpec {
        sbx: sbx.clone(),
        launch: acp_launch(profile, state),
        log: dirs.runtime_dir(&state.name).join("acp-agent.log"),
        paths: acp::PathMap::new(&state.mounts),
        cwd: state
            .mounts
            .primary
            .as_ref()
            .map(|m| container_path(&m.path.to_string_lossy()))
            .unwrap_or_else(|| config.sandbox_home().to_string()),
        profile: profile.clone(),
        sandbox: state.name.clone(),
        mcp: state.mcp.clone(),
        servers: if profile.network_prompts {
            serde_json::json!([network::mcp_server(config.sandbox_home())])
        } else {
            serde_json::json!([])
        },
        workspace: state.mounts.primary.as_ref().map(|m| m.path.clone()),
        mounts: {
            let (dirs, name) = (dirs.clone(), state.name.clone());
            Arc::new(move || {
                let state = state::load(&dirs, &name).ok().flatten();
                state.map(|s| s.mounts.all().map(|m| m.path.clone()).collect()).unwrap_or_default()
            })
        },
    };
    let bridge = (hooks.is_some() || profile.network_prompts)
        .then(|| Bridge::start_with(sbx.clone(), dirs.clone(), state.name.clone(), hooks));
    IN_SESSION.store(true, Ordering::SeqCst);
    let code = native::chat(spec).unwrap_or_else(|| Err(anyhow::anyhow!("the native session is gone")));
    IN_SESSION.store(false, Ordering::SeqCst);
    let summary = bridge.map(Bridge::finish).unwrap_or_default();
    drop(keep_alive);
    let code = code?;
    log_summary(&summary);
    log::step(format!("Session ended (exit code {code})"));
    Ok(code)
}

/// `sbx exec` of the harness's ACP server, in the primary workspace.
fn acp_launch(profile: &Profile, state: &SandboxState) -> Vec<OsString> {
    let command = profile.acp_command.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ");
    let mut cmd = args!["exec", "-i"];
    if let Some(primary) = &state.mounts.primary {
        cmd.extend(args!["--workdir", container_path(&primary.path.to_string_lossy())]);
    }
    let env = vertex_env(profile, &state.name).map(|pair| format!("{} ", shell_quote(&pair))).unwrap_or_default();
    cmd.extend(args![&state.name, "bash", "-lc", format!("{env}exec {command}")]);
    cmd
}

fn log_summary(summary: &network::Summary) {
    if !summary.allowed.is_empty() {
        log::info(format!("allowed during the session, for this sandbox: {}", summary.allowed.join(", ")));
    }
    if !summary.mounted.is_empty() {
        log::info(format!("mounted during the session: {}", summary.mounted.join(", ")));
    }
}

/// Starts the agent (its terminal UI, or its ACP server in ACP mode) and
/// waits for it. `keep_alive` is let go once the agent ends.
fn attach(
    sbx: &Sbx,
    dirs: &Dirs,
    args: &RunArgs,
    profile: &Profile,
    state: &SandboxState,
    keep_alive: Option<KeepAlive>,
) -> Result<ExitStatus> {
    let launch = if args.acp {
        if profile.acp_command.is_empty() {
            bail!(
                "harness '{}' has no ACP command; set harness.{}.acp_command in config.toml",
                profile.name,
                profile.name
            );
        }
        let command = profile.acp_command.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ");
        log::step(format!("Starting ACP agent: {command}"));
        acp_launch(profile, state)
    } else {
        log::step(format!("Starting {} (bypass permissions)", profile.name));
        let mut cmd = args!["run", "--name", &state.name];
        // sbx only sets the variable itself in sandboxes created after the secret.
        if let Some(pair) = vertex_env(profile, &state.name) {
            cmd.extend(args!["-e", pair]);
        }
        for pair in terminal_env(|name| env::var(name).ok()) {
            cmd.extend(args!["-e", pair]);
        }
        let extra = agent_args(profile, &args.agent_args);
        if !extra.is_empty() {
            if profile.bypass_args.is_empty() && !args.agent_args[0].starts_with('-') {
                log::warn(
                    "agent arguments that start with a bare word replace sbx's default flags; sand does not know this harness's bypass flag",
                );
            }
            cmd.push(OsString::from("--"));
            cmd.extend(extra.into_iter().map(OsString::from));
        }
        cmd
    };

    let runtime_dir = dirs.runtime_dir(&state.name);
    let refresher = (!args.acp).then(|| Refresher::start(sbx.clone(), runtime_dir, state.mcp.clone())).flatten();
    let bridge = profile.network_prompts.then(|| Bridge::start(sbx.clone(), dirs.clone(), state.name.clone()));
    IN_SESSION.store(true, Ordering::SeqCst);
    let status = match acp::PathMap::new(&state.mounts).filter(|_| args.acp) {
        Some(paths) => {
            let back = paths.clone();
            sbx.attach_filtered(launch, move |line| paths.to_sandbox(line), move |line| back.to_host(line))
        }
        None => sbx.attach(launch),
    };
    IN_SESSION.store(false, Ordering::SeqCst);
    drop(refresher);
    let summary = bridge.map(Bridge::finish).unwrap_or_default();
    drop(keep_alive);
    let status = status?;
    log_summary(&summary);
    log::step(format!("Session ended ({status})"));
    Ok(status)
}

/// The ACP session Tern starts for a session that handed its sandbox over
/// (`open_in_tern`): that one has just set the sandbox up and keeps it running
/// until the chat ends, so the agent is started right away.
fn attach_prepared(
    args: &RunArgs,
    dirs: &Dirs,
    profile: &Profile,
    state: &SandboxState,
    _lock: lock::SessionLock,
) -> Result<i32> {
    log::step(format!("sand: {} in sandbox '{}'", profile.name, state.name));
    log::ok("the session that opened this chat has set the sandbox up; starting the agent");
    let sbx = Sbx::prepared(false);
    let status = attach(&sbx, dirs, args, profile, state, None)?;
    Ok(status.code().unwrap_or(1))
}

/// Resolves `[HARNESS] [DIR]`, `--name` or `--scratch` to an existing sandbox
/// and the harness that was named or defaulted to.
pub fn find_sandbox(
    sbx: &Sbx,
    targets: &[String],
    name: Option<&str>,
    scratch: bool,
    dirs: &Dirs,
    config: &Config,
) -> Result<(String, String)> {
    let cwd = env::current_dir().context("determining the current directory")?;
    let (harness, specs) = split_targets(targets, config, &cwd, &dirs.home);
    let harness = harness.unwrap_or_else(|| config.default_harness().to_string());
    let name = match name {
        Some(name) => name.to_string(),
        None if scratch => state::sanitize_name(&format!("scratch-{harness}")),
        None => {
            let dir = specs.first().map(String::as_str).unwrap_or(".");
            let primary: PathBuf = Mount::parse(dir, &cwd, &dirs.home)?.path;
            state::default_name(dirs, &state::sanitize_name(&harness), &primary)?
        }
    };
    if !sbx.exists(&name)? {
        bail!("there is no sandbox named '{name}' (see `sbx ls`)");
    }
    Ok((name, harness))
}

/// Adds `mount` to a sandbox while its session runs, as `sand mount` does but
/// without a word on the terminal, which the agent owns. Answers whether
/// anything changed.
pub fn mount_in_session(sbx: &Sbx, dirs: &Dirs, name: &str, mount: &Mount) -> Result<bool> {
    let Some(mut state) = state::load(dirs, name)? else {
        bail!("'{name}' was not created by sand, so its mounts are unknown");
    };
    let requested = MountSet { primary: None, extras: vec![mount.clone()] };
    let plan = mounts::plan(&state.mounts, &requested, Strategy::Merge);
    if plan.is_noop() {
        // A note here is a change that cannot be made in place, such as the primary workspace's mode.
        if let Some(note) = plan.notes.first() {
            bail!("{note}");
        }
        return Ok(false);
    }
    for old in &plan.umount {
        sbx.run_quiet(args!["umount", name, old.umount_spec()])?;
    }
    for new in &plan.mount {
        sbx.run_quiet(args!["mount", name, new.mount_spec()])?;
    }
    state.mounts = plan.result;
    state::save(dirs, &state)?;
    Ok(true)
}

/// `sand mount` / `sand umount`: changes the extra mounts of an existing
/// sandbox in place. It takes no session lock, so it also works while an agent
/// runs in the sandbox, which sees the change right away.
pub fn change_mounts(args: MountArgs, add: bool, dirs: &Dirs, config: &Config) -> Result<i32> {
    let cwd = env::current_dir().context("determining the current directory")?;
    let (harness, specs) = split_targets(&args.targets, config, &cwd, &dirs.home);
    let verb = if add { "mount" } else { "unmount" };
    if specs.is_empty() {
        bail!("name at least one directory to {verb}");
    }
    let sbx = Sbx::connect(true)?;
    let harness: Vec<String> = harness.into_iter().collect();
    let (name, _) = find_sandbox(&sbx, &harness, args.name.as_deref(), args.scratch, dirs, config)?;
    let Some(mut state) = state::load(dirs, &name)? else {
        bail!("'{name}' was not created by sand, so its mounts are unknown; use `sbx {verb}` on it directly");
    };

    let plan = if add {
        let list = MountSet::from_list(specs.iter().map(|s| Mount::parse(s, &cwd, &dirs.home)).collect::<Result<_>>()?);
        let requested = MountSet { primary: None, extras: list.all().cloned().collect() };
        warn_exposing(dirs, requested.all());
        mounts::plan(&state.mounts, &requested, Strategy::Merge)
    } else {
        let paths: Vec<PathBuf> = specs.iter().map(|s| Mount::resolve_path(s, &cwd, &dirs.home)).collect();
        mounts::plan_umount(&state.mounts, &paths)?
    };
    if plan.is_noop() {
        for note in &plan.notes {
            log::warn(note);
        }
        log::ok(format!("nothing to {verb} in sandbox '{name}'"));
        return Ok(0);
    }

    // The host directories behind saved mounts must exist before sbx starts the sandbox.
    for mount in &state.internal_mounts {
        let _ = fs::create_dir_all(&mount.host);
    }
    let keep_alive = sbx.keep_alive(&name)?;
    sbx.wait_ready(&name, READY_TIMEOUT)?;
    apply_plan(&sbx, dirs, &mut state, plan)?;
    drop(keep_alive);
    show_set(&format!("mounts of '{name}'"), &state.mounts);
    if let Some(holder) = lock::holder(dirs, &name) {
        log::info(format!("the running session ({holder}) sees the change right away"));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_word_is_a_harness_unless_it_is_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        fs::create_dir(cwd.join("src")).unwrap();
        fs::create_dir(cwd.join("codex")).unwrap();
        let config = Config::default();
        let split = |words: &[&str]| {
            let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            split_targets(&words, &config, cwd, cwd)
        };
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(split(&[]), (None, vec![]));
        assert_eq!(split(&["codex"]), (Some("codex".into()), vec![]));
        assert_eq!(split(&["codex", ".", "../x:ro"]), (Some("codex".into()), s(&[".", "../x:ro"])));
        assert_eq!(split(&["."]), (None, s(&["."])));
        assert_eq!(split(&["src", "other"]), (None, s(&["src", "other"])));
        assert_eq!(split(&["src:ro"]), (None, s(&["src:ro"])));
        assert_eq!(split(&["./codex"]), (None, s(&["./codex"])));
        assert_eq!(split(&["my-kit", "."]), (Some("my-kit".into()), s(&["."])));
        assert_eq!(split(&["../missing"]), (None, s(&["../missing"])));
    }

    #[test]
    fn recreate_offers_only_what_the_command_line_leaves_out() {
        let mount = |path: &str, mode| Mount { path: path.into(), mode };
        let req = Request {
            profile: Profile::resolve("codex", &Config::default()).unwrap(),
            name: "codex-proj".into(),
            kind: Kind::Project,
            mounts: MountSet { primary: Some(mount("/p", Mode::Rw)), extras: vec![mount("/lib", Mode::Rw)] },
            explicit_mounts: true,
            mcp: vec!["gitlab".into()],
            template: None,
        };
        let mut state = SandboxState {
            name: "codex-proj".into(),
            harness: "codex".into(),
            agent: "codex".into(),
            kind: Kind::Project,
            mounts: MountSet {
                primary: Some(mount("/p", Mode::Rw)),
                extras: vec![mount("/lib", Mode::Ro), mount("/docs", Mode::Ro)],
            },
            mcp: vec!["gitlab".into(), "atlassian".into()],
            template: None,
            image_digest: None,
            declined_digest: None,
            internal_mounts: vec![],
            vertex_secret: None,
            created_at: 0,
        };
        assert_eq!(
            keepable(&req, Some(&state), true),
            [Keep::History, Keep::Mounts(vec![mount("/docs", Mode::Ro)]), Keep::Mcp(vec!["atlassian".into()])]
        );
        assert_eq!(Keep::Mounts(vec![mount("/docs", Mode::Ro)]).label(), "mounts: /docs (ro)");
        // A sandbox sand did not create has only its history to keep.
        assert_eq!(keepable(&req, None, true), [Keep::History]);
        state.mounts.extras.truncate(1);
        state.mcp.truncate(1);
        assert!(keepable(&req, Some(&state), false).is_empty());
    }

    #[test]
    fn history_counts_when_it_holds_a_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        assert!(!has_files(dir.path()));
        fs::write(dir.path().join("a/b/s.jsonl"), "").unwrap();
        assert!(has_files(dir.path()));
        assert!(!has_files(&dir.path().join("missing")));
    }

    #[test]
    fn sandbox_names_are_validated() {
        for good in ["ab", "my-project", "a.b", "Proj2"] {
            validate_name(good).unwrap();
        }
        for bad in ["a", "-ab", "a b", "a/b", "default", ""] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_host_terminal_is_made_known_to_the_agent() {
        let host = |name: &str| match name {
            "TERM_PROGRAM" => Some("ghostty".to_string()),
            "KITTY_WINDOW_ID" => Some(String::new()),
            "HOME" => Some("/Users/me".to_string()),
            _ => None,
        };
        assert_eq!(terminal_env(host), vec!["TERM_PROGRAM=ghostty"]);
        let over_ssh = |name: &str| (name == "TERM").then(|| "xterm-ghostty".to_string());
        assert_eq!(terminal_env(over_ssh), vec!["TERM_PROGRAM=ghostty"]);
        let kitty = |name: &str| (name == "TERM").then(|| "xterm-kitty".to_string());
        assert_eq!(terminal_env(kitty), vec!["TERM_PROGRAM=kitty"]);
        let plain = |name: &str| (name == "TERM").then(|| "xterm-256color".to_string());
        assert!(terminal_env(plain).is_empty());
    }

    #[test]
    fn bypass_flags_survive_bare_word_arguments() {
        let claude = Profile::resolve("claude", &Config::default()).unwrap();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(agent_args(&claude, &[]).is_empty());
        assert_eq!(agent_args(&claude, &s(&["--continue"])), s(&["--continue"]));
        assert_eq!(
            agent_args(&claude, &s(&["fix the build"])),
            s(&["--dangerously-skip-permissions", "fix the build"])
        );
    }
}
