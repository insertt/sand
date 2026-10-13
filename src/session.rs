//! The main flow: find or create the sandbox, reconcile mounts and MCP servers,
//! copy the host configuration in, and hand the terminal to the agent.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use console::Term;
use dialoguer::Select;

use crate::acp;
use crate::args;
use crate::cli::{ConflictChoice, RunArgs, StatusArgs};
use crate::config::{Config, StatuslineKind};
use crate::harness::{self, Profile};
use crate::lock;
use crate::log;
use crate::mcp;
use crate::mounts::{self, Mode, Mount, MountSet, Plan, Strategy};
use crate::paths::{Dirs, RUNTIME_DIR_NAME, container_path};
use crate::registry;
use crate::runtime::{self, Refresher};
use crate::sbx::Sbx;
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

fn shell_quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@=+".contains(c)) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
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
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "{question}\nThis needs a decision but there is no terminal to ask on; pass --on-conflict replace|merge|abort."
        );
    }
    let labels: Vec<&str> = items.iter().map(|(_, label)| label.as_str()).collect();
    let picked = Select::new().with_prompt(question).items(&labels).default(0).interact_on_opt(&Term::stderr())?;
    Ok(picked.map_or(ConflictChoice::Abort, |i| items[i].0))
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
    } else if args.acp || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
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
        let picked = Select::new()
            .with_prompt("How do you want to continue?")
            .items(&labels)
            .default(0)
            .interact_on_opt(&Term::stderr())?;
        picked.map_or(ConflictChoice::Abort, |i| items[i].0)
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

/// Registers the Vertex AI access token as a custom secret of the sandbox. sbx
/// runs the token command on the host and keeps the token fresh; the sandbox
/// only ever holds the placeholder.
fn register_vertex_secret(sbx: &Sbx, profile: &Profile, name: &str) -> Result<()> {
    let Some(vertex) = &profile.vertex else { return Ok(()) };
    log::step("Registering the Vertex AI access token with sbx");
    log::info(format!("project {}, region {}", vertex.project, vertex.region));
    log::info(format!(
        "token: `{}` on the host, refresh {}, sent to {}",
        vertex.token_command,
        vertex.refresh,
        vertex.hosts.join(", ")
    ));
    sbx.run(vertex.secret_args(name, profile.vertex_token_env.as_deref())).with_context(|| {
        format!(
            "registering the token secret (does `{}` print a token on the host? \
             `sbx secret set-custom --show-error` shows why sbx rejected it)",
            vertex.token_command
        )
    })?;
    log::ok("the sandbox proxy adds the token to Vertex AI requests; it never enters the sandbox");
    Ok(())
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
    sbx.run(cmd)?;
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
        created_at: state::now(),
    };
    state::save(dirs, &state)?;
    log::ok("sandbox created");
    Ok(state)
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

/// Mounts sand's own directories: the runtime dir (status line, staged config)
/// and, unless the sandbox is disposable, host-backed history directories.
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
    log::step("Mounting sand's status and history directories");
    let targets = missing.iter().map(|m| shell_quote(&m.target)).collect::<Vec<_>>().join(" ");
    sbx.exec_sh(&state.name, &format!("mkdir -p {targets}"))?;
    for mount in missing {
        let what = if mount.mode == Mode::Ro { "status" } else { "history" };
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
    let staged = sync::stage(profile, &dirs.home, home, &runtime.join("sync"), settings)?;
    fs::write(runtime.join("apply.sh"), sync::apply_script(home))?;
    log::info(format!(
        "{} file(s) staged, {} changed since the last launch; credentials are never copied",
        staged.total, staged.updated
    ));
    sbx.run(args!["exec", name, "sh", format!("{home}/{RUNTIME_DIR_NAME}/apply.sh")])?;
    log::ok("configuration applied");
    Ok(())
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

    if let Some(editor) = args.install_editor {
        crate::editor::install(editor, dirs, harness.as_deref())?;
        return Ok(0);
    }

    let req = build_request(&args, dirs, config, harness, specs)?;
    let profile = &req.profile;
    let sbx = Sbx::new(!args.acp);

    log::step(format!("sand: {} in sandbox '{}'", profile.name, req.name));
    if profile.is_basic() {
        log::info(format!(
            "'{}' is not a harness sand has a profile for; running it without config copy, history or status line",
            profile.name
        ));
    }
    log::info(format!("using {}", sbx.version()?));

    let description = match &req.mounts.primary {
        Some(primary) => format!("{} on {}", profile.name, primary.path.display()),
        None => format!("{} scratch session", profile.name),
    };
    let _lock = lock::acquire(dirs, &req.name, &description)?;
    log::ok("session lock acquired");

    // --- find out what exists -------------------------------------------------
    log::step("Looking for an existing sandbox");
    let mut exists = sbx.exists(&req.name)?;
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
        bail!(
            "nothing to resume: there is no sandbox named '{}'. Run without --resume to create it{}.",
            req.name,
            if args.name.is_none() { ", or pass --name to pick another sandbox" } else { "" }
        );
    }
    if args.recreate && exists {
        remove_sandbox(&sbx, dirs, &req.name, !args.no_preserve_history)?;
        exists = false;
        current = None;
    } else if args.recreate && args.no_preserve_history {
        state::forget(dirs, &req.name, false)?;
    }

    if let (true, false, Some(state)) = (exists, args.resume, current.as_mut()) {
        match nightly_update(&req, dirs, state, &args)? {
            ConflictChoice::Merge => {}
            ConflictChoice::Replace => {
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
        if req.explicit_mounts {
            log::warn("--resume attaches to the sandbox as it is; the mounts on the command line are ignored");
        }
        let load_mcp = match &current {
            Some(state) => req.mcp.iter().filter(|m| !state.mcp.contains(m)).cloned().collect(),
            None => req.mcp.clone(),
        };
        let keep = current.as_ref().map(|s| s.mounts.clone()).unwrap_or_default();
        Decision::Reuse { plan: mounts::plan(&keep, &keep, Strategy::Merge), load_mcp }
    } else {
        match &current {
            Some(state) => reconcile(&req, state, &args)?,
            None => reconcile_unmanaged(&req, &args)?,
        }
    };

    let (mut state, plan, load_mcp) = match decision {
        Decision::Abort => {
            log::info("aborted; nothing was changed");
            return Ok(1);
        }
        Decision::Create => {
            if exists {
                remove_sandbox(&sbx, dirs, &req.name, true)?;
            }
            // Before the sandbox exists, so that sbx sets the token variable in it.
            register_vertex_secret(&sbx, profile, &req.name)?;
            let state = create_sandbox(&sbx, dirs, config, &req)?;
            // Extra mounts of a mountless sandbox still have to be attached.
            let pending = mounts::plan(&state.mounts, &req.mounts, Strategy::Merge);
            (state, pending, Vec::new())
        }
        Decision::Reuse { plan, load_mcp } => {
            let state = current.unwrap_or_else(|| SandboxState {
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
                created_at: state::now(),
            });
            log::step(format!("Using existing sandbox '{}'", req.name));
            register_vertex_secret(&sbx, profile, &req.name)?;
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

    sync_config(&sbx, dirs, config, profile, &state.name)?;

    log::step("Checking agent API and MCP servers");
    let runtime_dir = dirs.runtime_dir(&state.name);
    runtime::write_info(&runtime_dir, &state.name, profile)?;
    let mcp_status = mcp::status(&sbx, &state.mcp);
    runtime::write_mcp_status(&runtime_dir, &mcp_status)?;
    let api = runtime::check_api(&sbx, &state.name, profile);
    runtime::banner(&state.name, profile, &state.mounts, api, &mcp_status);
    if args.no_attach {
        log::ok(format!("sandbox '{}' is ready; `sand {} --resume` attaches to it", state.name, profile.name));
        return Ok(0);
    }
    if profile.statusline != StatuslineKind::Claude && !args.acp {
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
        let mut cmd = args!["exec", "-i"];
        if let Some(primary) = &state.mounts.primary {
            cmd.extend(args!["--workdir", container_path(&primary.path.to_string_lossy())]);
        }
        let env = vertex_env(profile, &state.name).map(|pair| format!("{} ", shell_quote(&pair))).unwrap_or_default();
        cmd.extend(args![&state.name, "bash", "-lc", format!("{env}exec {command}")]);
        cmd
    } else {
        log::step(format!("Starting {} (bypass permissions)", profile.name));
        let mut cmd = args!["run", "--name", &state.name];
        // sbx only sets the variable itself in sandboxes created after the secret.
        if let Some(pair) = vertex_env(profile, &state.name) {
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

    let refresher = (!args.acp).then(|| Refresher::start(sbx.clone(), runtime_dir, state.mcp.clone())).flatten();
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
    drop(keep_alive);
    let status = status?;

    log::step(format!("Session ended ({status})"));
    if state.kind == Kind::Dedicated {
        log::info("this was a dedicated scratch sandbox; cleaning it up");
        if let Err(err) = remove_sandbox(&sbx, dirs, &state.name, false) {
            log::warn(format!("could not remove it: {err:#}. Remove it with `sbx rm {}`.", state.name));
        }
    } else {
        log::info(format!("sandbox '{}' is kept; `sand {} --resume` attaches to it again", state.name, profile.name));
    }
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

/// `sand status`: the banner for an existing sandbox.
pub fn status(args: StatusArgs, dirs: &Dirs, config: &Config) -> Result<i32> {
    let sbx = Sbx::new(true);
    let (name, harness) = find_sandbox(&sbx, &args.targets, args.name.as_deref(), args.scratch, dirs, config)?;
    let (profile, mounts, servers) = match state::load(dirs, &name)? {
        Some(state) => (Profile::resolve(&state.harness, config)?, state.mounts, state.mcp),
        None => {
            log::warn(format!("'{name}' was not created by sand; mounts and MCP servers are unknown"));
            (Profile::resolve(&harness, config)?, MountSet::default(), Vec::new())
        }
    };
    let mcp_status = mcp::status(&sbx, &servers);
    let api = runtime::check_api(&sbx, &name, &profile);
    runtime::banner(&name, &profile, &mounts, api, &mcp_status);
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
    fn sandbox_names_are_validated() {
        for good in ["ab", "my-project", "a.b", "Proj2"] {
            validate_name(good).unwrap();
        }
        for bad in ["a", "-ab", "a b", "a/b", "default", ""] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
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

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("npx"), "npx");
        assert_eq!(shell_quote("@agentclientprotocol/claude-agent-acp"), "@agentclientprotocol/claude-agent-acp");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
