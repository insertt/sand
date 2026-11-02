//! Draws the session in the shape of Tern's chat skin (the roles omp's
//! transcript and composer carry, which Tern styles itself), after
//! tern-agents-ui: setup as a rail of steps that folds once the agent starts,
//! your messages as cards, each agent turn grouped with its thoughts, inline
//! tool cards and a usage footer, and in the dock the plan, a working line and
//! a floating composer with a context bar and the status line's facts:
//! sandbox, branch, API, MCP servers, mode, model and cost. A function of the
//! state alone: the SDK sends only what changed.

use std::time::Duration;

use serde_json::{Value, json};
use tern_sdk::ui::{
    self, Ask, AskAlways, Bound, ChecklistItem, Extent, Node, Phase, Span, Thresholds, ToolButton, ToolPreview,
    TraceLine, View,
};

use super::commands;
use super::composer::Composer;
use super::model::{
    Approval, Entry, Permission, Question, SessionPicker, Setting, Setup, StepStatus, TargetKind, ToolCall,
    ToolContent, ToolStatus, Transcript, Turn, TurnUsage,
};
use crate::mcp::{McpState, McpStatus};
use crate::runtime::ApiState;

/// The composer's look, after omp's floating composer: a card at the bottom of
/// the column, the editor over a bar of chips. Tern scopes it to the surface.
pub const STYLE: &str = r#"
[data-role='sand.composer'] {
	width: calc(100% - 24px);
	max-width: 736px;
	margin: 0 auto 10px !important;
	padding: 8px 10px 6px;
	box-sizing: border-box;
	gap: 6px;
	border-radius: 14px;
	background: var(--card);
	box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--t1) 12%, transparent), 0 10px 30px -18px rgba(0, 0, 0, 0.35);
	font-family: var(--sans);
}
[data-role='sand.composer'] > .sf-editor {
	min-height: calc(2 * var(--sf-lh));
	background: transparent;
	box-shadow: none;
}
[data-role='sand.composer.context'] { margin: 0 4px; }
[data-role='sand.composer.bar'] { gap: 8px; min-width: 0; font: 12.5px/1.3 var(--sans); }
[data-role='sand.composer.bar'] > * { flex: none; }
[data-role='sand.composer.mcp'] .sf-btn-label, [data-role='sand.composer.mcp'] .ico { color: var(--tc); }
[data-role='sand.composer.facts'] {
	gap: 4px 14px;
	padding: 4px 4px 0;
	border-top: 1px solid color-mix(in srgb, var(--t1) 8%, transparent);
	font: 12px/1.3 var(--sans);
	color: var(--t3);
}
:is([data-role='sand.composer.model'], [data-role='sand.composer.effort'], [data-role='sand.composer.mode']).sf-row {
	padding: 2px 8px;
	border-radius: 999px;
	box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--t1) 12%, transparent);
}
[data-role='sand.composer.usage'] { font-family: var(--mono); }
:is([data-role='sand.composer.send'], [data-role='sand.composer.stop']) {
	padding: 3px 8px;
	border-radius: 8px;
	background: var(--l2);
}
[data-role='sand.notes'], [data-role='sand.queue'] {
	width: calc(100% - 24px);
	max-width: 736px;
	margin: 0 auto 6px !important;
	padding: 6px 10px;
	box-sizing: border-box;
	border-radius: 10px;
	background: var(--l2);
	font: 12.5px/1.4 var(--sans);
}
/* Room under the last turn, so that its footer clears the floating composer. */
.sf-spacer[data-role='sand.tail'] { height: calc(2 * var(--sf-lh, 18px)) !important; }
/* The composer sits at the pane's bottom: its menus open upward, over the chip. */
[data-role='sand.composer'] .sf-menu > .sf-menu-pop {
	top: auto !important;
	bottom: calc(100% + 4px) !important;
	max-height: 60vh;
	overflow-y: auto;
}
/* A tool's head: its label centered on its icon, however many lines it takes. */
.sf-tool > .sf-tool-head { align-items: center; }
.sf-tool > .sf-tool-head > :is(.sf-tool-ic, .sf-tool-line, .sf-tool-time, .sf-tool-st, .sf-chev) { align-self: center; }
.sf-tool > .sf-tool-head > .sf-tool-ic { display: inline-flex; align-items: center; }
[data-role='omp.run.status'] { align-items: center; }
.sf-row[data-role='omp.working'] {
	width: calc(100% - 24px);
	max-width: 736px;
	margin: 0 auto 6px !important;
	padding: 0 4px !important;
	box-sizing: border-box;
}
"#;

/// The composer's id: region `dock`, then `composer`, then `input`.
pub const EDITOR: &str = "dock.composer.input";

/// Lines of context around a change in a diff.
const CONTEXT: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// Tern (or Carly) submits text through the composer.
    Send(String),
    /// The send button.
    Submit,
    /// The stop button.
    Stop,
    /// A click asked for the keys.
    Focus,
    /// An approval button of the tool waiting for permission.
    Answer(Approval),
    /// An option of setup's question.
    Choose(usize),
    /// A choice picked in the model or effort chip.
    Pick(Setting, String),
    /// A click on a completion of the draft.
    Complete(usize),
    /// A session picked in the session picker, or highlighted there.
    Session(usize),
    HoverSession(usize),
    ClosePicker,
    /// Native editing: UTF-16 `[from, to)` of the draft replaced with `text`.
    Edit {
        from: usize,
        to: usize,
        text: String,
        cursor: usize,
        len: usize,
    },
    Undo,
    /// A queued follow-up taken back, and the paused queue's two choices.
    Unqueue(usize),
    QueueContinue,
    QueueDiscard,
    DismissNotes,
    /// The welcome card's Continue, and its suggested prompts.
    ContinueLast,
    Suggest(String),
}

/// What the session runs in: what the status line shows besides the agent's own facts.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub harness: String,
    /// The agent's product name (`Claude Code`).
    pub product: String,
    pub sandbox: String,
    /// The primary workspace as the person knows it (`~/src/app`).
    pub workspace: Option<String>,
    /// The mounts, one line each.
    pub mounts: Vec<String>,
    pub mcp: Vec<McpStatus>,
    pub api: Option<ApiState>,
    /// The host the API check reaches.
    pub api_host: Option<String>,
    pub branch: Option<String>,
    /// The model and effort the host's settings pick, until the agent says.
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Info {
    fn model<'a>(&'a self, t: &'a Transcript) -> &'a str {
        t.model_name().or(self.model.as_deref()).unwrap_or(self.product.as_str())
    }

    fn effort<'a>(&'a self, t: &'a Transcript) -> Option<&'a str> {
        t.effort_name().or(self.effort.as_deref())
    }
}

pub fn view(t: &Transcript, composer: &Composer, info: &Info) -> View<Msg> {
    View::new()
        .main(ui::col().children(transcript(t, info)))
        .dock(ui::col().children(dock(t, composer, info)))
        .layer(ui::col().children(layer(t, composer, info)))
}

/// The item a list event names: its node id ends in the item's key, `<letter><index>`.
fn item_index(item: &str) -> usize {
    item.rsplit('.').next().and_then(|key| key.get(1..)).and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// What floats over the session: the completions of a `/command` being
/// typed, under the caret, and the session picker.
fn layer(t: &Transcript, composer: &Composer, info: &Info) -> Vec<Node<Msg>> {
    if let Some((_, key)) = &t.exit_armed {
        let what = if t.turn == Turn::Setup { "stop setting up" } else { "exit" };
        // As long as the second press is awaited.
        return vec![ui::toast(format!("Press {key} again to {what}")).key("exit").ttl(2000u64).into()];
    }
    if let Some(picker) = &t.picker {
        return vec![session_picker(picker, info)];
    }
    let completions = if composer.dismissed { Vec::new() } else { commands::complete(&composer.draft, &t.commands) };
    if completions.is_empty() {
        return Vec::new();
    }
    let selected = composer.pick.min(completions.len() - 1);
    let items = completions.iter().enumerate().map(|(i, command)| {
        let mut item = ui::item(format!("/{}", command.name)).key(format!("c{i}")).detail(command.description.clone());
        if let Some(hint) = &command.hint {
            item = item.value(hint.clone());
        }
        if command.builtin {
            item = item.icon("box");
        }
        item
    });
    let list = ui::list()
        .key("list")
        .selected(format!("layer.complete.list.c{selected}"))
        .max_lines(10)
        .children(items)
        .on_select(|item| Msg::Complete(item_index(item)))
        .on_activate(|item| Msg::Complete(item_index(item)));
    vec![
        ui::overlay()
            .key("complete")
            .role("omp.autocomplete")
            .anchor(ui::Anchor::Caret(EDITOR.to_string()))
            .size("sm")
            .child(list)
            .into(),
    ]
}

/// The sessions `/sessions` and `/resume` offer, newest first as the agent lists them.
fn session_picker(picker: &SessionPicker, info: &Info) -> Node<Msg> {
    let empty = match (&picker.error, picker.loading) {
        (Some(error), _) => error.clone(),
        (None, true) => format!("Asking {} for its sessions…", info.product),
        (None, false) => "No sessions".to_string(),
    };
    let items = picker.sessions.iter().enumerate().map(|(i, session)| {
        let mut detail = Vec::new();
        detail.extend(session.updated.as_deref().map(|u| u.replace('T', " ").chars().take(16).collect::<String>()));
        detail.extend(session.cwd.clone());
        ui::item(session.label())
            .key(format!("s{i}"))
            .icon("history")
            .detail(detail.join(" · "))
            .value(session.id.chars().take(8).collect::<String>())
    });
    let selected = picker.selected.min(picker.sessions.len().saturating_sub(1));
    let list = ui::list()
        .key("list")
        .selected(format!("layer.sessions.list.s{selected}"))
        .empty(empty)
        .max_lines(12)
        .children(items)
        .on_select(|item| Msg::HoverSession(item_index(item)))
        .on_activate(|item| Msg::Session(item_index(item)));
    let hint = ui::text(spans(&[("↑↓ choose · Enter resumes · Esc closes", "muted")])).key("hint");
    ui::overlay()
        .key("sessions")
        .role("sand.sessions")
        .anchor(ui::Anchor::Center)
        .size("md")
        .head(format!("Resume a {} session", info.product))
        .end(ui::button("Close").key("close").variant("quiet").size("sm").keys(["esc"]).on_click(Msg::ClosePicker))
        .child(list)
        .foot(hint)
        .into()
}

fn spans(parts: &[(&str, &str)]) -> Vec<Span> {
    parts
        .iter()
        .map(|(text, style)| if style.is_empty() { Span::new(*text) } else { Span::new(*text).style(*style) })
        .collect()
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn duration(d: Duration) -> String {
    match d.as_millis() {
        ms if ms < 1000 => format!("{ms}ms"),
        ms if ms < 60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        ms => format!("{}m {}s", ms / 60_000, (ms / 1000) % 60),
    }
}

fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3).replace(".0k", "k"),
        n => n.to_string(),
    }
}

/// `18000` as `18,000`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn cost(usd: f64) -> String {
    if usd < 0.005 { "<$0.01".to_string() } else { format!("${usd:.2}") }
}

fn transcript(t: &Transcript, info: &Info) -> Vec<Node<Msg>> {
    let mut nodes = Vec::new();
    if !t.setup.steps.is_empty() {
        nodes.push(setup(&t.setup, info));
    }
    if t.entries.is_empty() && !matches!(t.turn, Turn::Setup) {
        nodes.push(welcome(t, info));
    }
    // Everything the agent does until the next message is one turn.
    let mut turn: Vec<Node<Msg>> = Vec::new();
    let mut turn_key = String::new();
    let flush = |nodes: &mut Vec<Node<Msg>>, turn: &mut Vec<Node<Msg>>, key: &mut String| {
        if !turn.is_empty() {
            nodes.push(ui::col().key(key.clone()).role("omp.assistant").children(std::mem::take(turn)).into());
        }
        key.clear();
    };
    let last = t.entries.len().saturating_sub(1);
    for (i, entry) in t.entries.iter().enumerate() {
        let key = format!("e{i}");
        if let Entry::User(text) = entry {
            flush(&mut nodes, &mut turn, &mut turn_key);
            nodes.push(user(key, text));
            continue;
        }
        if let Entry::Resumed(title) = entry {
            flush(&mut nodes, &mut turn, &mut turn_key);
            let label = if title.is_empty() { "Resumed".to_string() } else { format!("Resumed · {title}") };
            nodes.push(ui::rule().key(key).role("agent.resumed").label(label).into());
            continue;
        }
        if turn_key.is_empty() {
            turn_key = format!("turn{i}");
        }
        let streaming = i == last && t.turn == Turn::Working;
        turn.push(match entry {
            Entry::User(_) | Entry::Resumed(_) => unreachable!("handled above"),
            Entry::Agent(text) => ui::md(text.clone()).key(key).prop("stream", streaming).into(),
            Entry::Thought(thought) => {
                let head = match thought.took {
                    None => vec![Span::new("Thinking…").style("muted").fx("shimmer")],
                    Some(took) => spans(&[(&format!("Thought for {}", duration(took)), "muted")]),
                };
                let role = if thought.took.is_none() { "omp.thinking.live" } else { "omp.thinking" };
                ui::section()
                    .key(key)
                    .role(role)
                    .head(head)
                    .collapsible(true)
                    .collapsed(true)
                    .child(ui::md(thought.text.clone()).key("t"))
                    .into()
            }
            Entry::Tool(tool) => tool_node(key, tool, t.permission.as_ref().filter(|p| p.tool == tool.id)),
            Entry::Notice(text) => ui::text(spans(&[(text, "warning")])).key(key).role("omp.notice").into(),
            Entry::Error { text, detail } => error(key, text, detail.as_deref()),
            Entry::TurnEnd(usage) => usage_footer(key, usage),
        });
    }
    flush(&mut nodes, &mut turn, &mut turn_key);
    // Room under the last turn, so that its end and its tooltips clear the floating composer.
    // (A spacer: Tern hides an empty row, whatever its size.)
    nodes.push(ui::node("spacer").key("tail").role("sand.tail").prop("size", "lg"));
    nodes
}

/// Setup as a rail of steps while it runs; one folded summary line once the agent starts.
fn setup(setup: &Setup, info: &Info) -> Node<Msg> {
    let lines: Vec<TraceLine> = setup
        .steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let mut line = TraceLine::new(format!("s{i}"), step.title.clone());
            line.status = Some(
                match step.status {
                    StepStatus::Running => "running",
                    StepStatus::Done | StepStatus::Warned => "done",
                    StepStatus::Failed => "error",
                }
                .into(),
            );
            line.icon = match step.status {
                StepStatus::Warned => Some("warn".into()),
                _ => None,
            };
            let detail = step.waiting.as_ref().or(step.notes.last());
            line.detail = detail.map(|d| d.clone().into());
            match step.took {
                Some(took) => line.took = Some(millis(took)),
                None => line.age = Some(0),
            }
            line
        })
        .collect();
    // A step's notes and what a failed command printed open under it.
    let details = setup.steps.iter().enumerate().filter_map(|(i, step)| {
        let mut text = step.notes.join("\n");
        if let Some(output) = &step.output {
            text = format!("{text}\n{}", output.trim_end()).trim_start().to_string();
        }
        (step.notes.len() > 1 || step.output.is_some()).then(|| ui::code(text).lang("text").key(format!("s{i}")))
    });
    let trace = ui::trace().key("setup").role("sand.setup").lines(lines).children(details);
    match setup.took {
        None => trace
            .variant("rail")
            .title(spans(&[("Setting up ", "muted"), (&info.sandbox, "accent")]))
            .status(if setup.failed() { "error" } else { "running" })
            .into(),
        Some(took) => {
            let summary = format!("Set up {} in {} · {} steps", info.sandbox, duration(took), setup.steps.len());
            trace
                .variant("summary")
                .summary(summary)
                .status(if setup.failed() { "error" } else { "done" })
                .took(millis(took))
                .into()
        }
    }
}

/// What greets the person before the first message, as omp's welcome card.
fn welcome(t: &Transcript, info: &Info) -> Node<Msg> {
    let mut where_ = vec![Span::new(info.model(t)).style("accent")];
    if let Some(effort) = info.effort(t) {
        where_.push(Span::new(format!(" · {effort}")).style("accent"));
    }
    where_.push(Span::new("  ·  ").style("muted"));
    where_.push(Span::new(format!("sandbox {}", info.sandbox)).style("strong"));
    if let Some(workspace) = &info.workspace {
        where_.push(Span::new(format!("  ·  {workspace}")).style("muted"));
    }
    let mut facts = ui::kv().key("facts");
    for (i, mount) in info.mounts.iter().enumerate() {
        facts = facts.item(if i == 0 { "mounts" } else { "" }, Span::new(mount.clone()).style("path"));
    }
    facts = facts.item("API", api_spans(info)).item("MCP", mcp_spans(&info.mcp));
    let tips: Vec<Span> = [("⇧Enter", "new line"), ("⇧Tab", "mode"), ("Esc", "stop"), ("Ctrl+C ×2", "quit")]
        .iter()
        .enumerate()
        .flat_map(|(i, (key, text))| {
            let gap = (i > 0).then(|| Span::new("   "));
            gap.into_iter().chain([Span::new(*key).style("key"), Span::new(format!(" {text}")).style("muted")])
        })
        .collect();
    ui::card()
        .key("welcome")
        .role("omp.welcome")
        .child(
            ui::row()
                .key("lockup")
                .role("omp.welcome.lockup")
                .align("center")
                .gap("sm")
                .child(ui::icon(format!("app-{}", info.harness).as_str()).tone("accent").role("omp.welcome.logo"))
                .child(ui::text(spans(&[(&info.product, "strong")])).role("omp.welcome.wordmark"))
                .child(
                    ui::text(spans(&[(&format!("in sand {}", env!("CARGO_PKG_VERSION")), "muted")]))
                        .role("omp.welcome.version"),
                ),
        )
        .child(ui::text(where_).key("where").role("agent.where"))
        .child(facts)
        .child(starters(t))
        .child(ui::text(tips).key("tips").role("omp.welcome.tip"))
        .into()
}

/// Prompts to start with, and the last conversation to continue.
const SUGGESTIONS: &[&str] = &["Explain this codebase", "Find and fix a bug", "Review my uncommitted changes"];

fn starters(t: &Transcript) -> Node<Msg> {
    let mut row = ui::row().key("starters").role("agent.starters").gap("sm").wrap(true);
    if let Some(last) = &t.last_session {
        let label: String = last.label().chars().take(48).collect();
        row = row.child(
            ui::button(format!("Continue “{label}”"))
                .key("continue")
                .variant("primary")
                .size("sm")
                .icon("history")
                .on_click(Msg::ContinueLast),
        );
    }
    for (i, prompt) in SUGGESTIONS.iter().enumerate() {
        row = row.child(ui::button(*prompt).key(i).size("sm").on_click(Msg::Suggest(prompt.to_string())));
    }
    row.into()
}

fn api_spans(info: &Info) -> Vec<Span> {
    let host = info.api_host.clone().unwrap_or_else(|| "API".to_string());
    match info.api {
        Some(ApiState::Ok) => vec![Span::new("✓ ").style("success"), Span::new(format!("{host} reachable"))],
        Some(ApiState::Blocked) => {
            vec![
                Span::new("✗ ").style("error"),
                Span::new(format!("{host} blocked by the network policy")).style("error"),
            ]
        }
        Some(ApiState::Unreachable) => {
            vec![Span::new("✗ ").style("error"), Span::new(format!("{host} unreachable")).style("error")]
        }
        Some(ApiState::NotConfigured | ApiState::Skipped) | None => vec![Span::new("not checked").style("muted")],
    }
}

fn mcp_spans(mcp: &[McpStatus]) -> Vec<Span> {
    if mcp.is_empty() {
        return vec![Span::new("none").style("muted")];
    }
    mcp.iter()
        .enumerate()
        .flat_map(|(i, server)| {
            let (mark, style) = mcp_mark(server.state);
            let gap = (i > 0).then(|| Span::new("  "));
            gap.into_iter().chain([Span::new(format!("{} ", server.name)), Span::new(mark).style(style)])
        })
        .collect()
}

fn mcp_mark(state: McpState) -> (&'static str, &'static str) {
    match state {
        McpState::Ok => ("✓", "success"),
        McpState::NeedsAuth => ("⚠ auth", "warning"),
        McpState::Error => ("✗", "error"),
        McpState::Unknown => ("?", "muted"),
    }
}

/// A sent message, as omp's user card.
fn user(key: String, text: &str) -> Node<Msg> {
    ui::card().key(key).role("omp.user").child(ui::md(text.to_string()).key("text")).into()
}

fn error(key: String, text: &str, detail: Option<&str>) -> Node<Msg> {
    let mut card = ui::card()
        .key(key)
        .role("omp.error")
        .tone("error")
        .child(ui::text(spans(&[(text, "error strong")])).key("head").prop("wrap", "word"));
    if let Some(detail) = detail {
        card = card.child(ui::code(detail.to_string()).lang("text").key("detail"));
    }
    card.into()
}

/// The quiet line under a finished turn: time, tokens, cost, and why it stopped early.
fn usage_footer(key: String, usage: &TurnUsage) -> Node<Msg> {
    let mut parts = vec![duration(usage.took)];
    parts.extend(usage.tokens.map(|n| format!("{} tokens", thousands(n))));
    parts.extend(usage.cost.map(cost));
    let mut text = vec![Span::new(format!("{} this turn", parts.join(" · "))).style("muted")];
    if let Some(stopped) = &usage.stopped {
        text.push(Span::new(" · ").style("muted"));
        text.push(Span::new(stopped.clone()).style("warning"));
    }
    ui::row().key(key).role("omp.turn.usage").gap("xs").align("center").child(ui::text(text).key("t")).into()
}

/// The `tool` icon names Tern knows, by ACP tool kind.
fn tool_name(kind: &str) -> &str {
    match kind {
        "execute" => "bash",
        "read" => "read",
        "edit" => "edit",
        "delete" | "move" => "write",
        "search" => "grep",
        "fetch" => "fetch",
        "think" => "think",
        other => other,
    }
}

fn verb(kind: &str) -> &'static str {
    match kind {
        "execute" => "Bash",
        "read" => "Read",
        "edit" => "Edit",
        "delete" => "Delete",
        "move" => "Move",
        "search" => "Search",
        "fetch" => "Fetch",
        "think" => "Think",
        _ => "Tool",
    }
}

/// `+added −removed` over a tool's diffs.
fn diff_stat(content: &[ToolContent]) -> Option<String> {
    let (mut added, mut removed, mut any) = (0usize, 0usize, false);
    for item in content {
        if let ToolContent::Diff { old, new, .. } = item {
            any = true;
            let hunk = hunks(old.as_deref(), new);
            for line in hunk[0]["lines"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                match line.chars().next() {
                    Some('+') => added += 1,
                    Some('-') => removed += 1,
                    _ => {}
                }
            }
        }
    }
    any.then(|| format!("+{added} −{removed}"))
}

fn tool_node(key: String, tool: &ToolCall, permission: Option<&Permission>) -> Node<Msg> {
    let name = tool_name(&tool.kind);
    let mut node = ui::tool(name).key(key).role(format!("omp.tool.{name}"));
    // ACP titles often repeat the target, so the head reads verb and target,
    // and the agent's title is the tooltip.
    node = match &tool.target {
        Some((target, kind)) => {
            let kind = match kind {
                TargetKind::Command => "command",
                TargetKind::Path => "path",
                TargetKind::Pattern => "pattern",
                TargetKind::Query => "query",
            };
            node.title(verb(&tool.kind)).target(target.clone()).target_kind(kind).intent(tool.title.clone())
        }
        None if tool.title.is_empty() => node.title(verb(&tool.kind)),
        None => node.title(tool.title.clone()),
    };
    let failed = tool.status == ToolStatus::Failed;
    let status = match tool.status {
        _ if permission.is_some() => "ask",
        ToolStatus::Pending => "pending",
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Failed => "error",
        ToolStatus::Cancelled => "cancelled",
    };
    // As omp: a line of its own, framed only when it failed or asks.
    node = node.status(status).frame(if failed || permission.is_some() { "card" } else { "inline" });
    node = match tool.finished {
        // Tern counts on from the first `age` it gets; an unchanged one sends nothing.
        None => node.age(0i64),
        Some(end) => node.took(millis(end.saturating_duration_since(tool.started))),
    };
    let stat = diff_stat(&tool.content);
    if let Some(stat) = &stat {
        node = node.meta(vec![stat.clone().into()]);
    }
    if !tool.content.is_empty() {
        let (collapsed, preview) = match stat {
            // Edits stay open, clamped; other output folds once done.
            Some(_) => (false, ToolPreview::Lines(14)),
            None => (tool.status.settled() && !failed, ToolPreview::Tail(4)),
        };
        node = node.collapsible(true).collapsed(collapsed).preview(preview).tools(vec![ToolButton {
            id: "copy".into(),
            label: Some("Copy".into()),
            keys: Vec::new(),
        }]);
    }
    if let Some(approval) = tool.approval {
        node = node.approved(match approval {
            Approval::Once => "once",
            Approval::Always => "always",
            Approval::Denied => "denied",
        });
    }
    if let Some(permission) = permission {
        let label = |kinds: &[&str]| permission.option(kinds).map(|o| o.name.clone()).filter(|n| !n.is_empty());
        let always = match label(&["allow_always"]) {
            Some(name) => AskAlways::Label(name),
            None => AskAlways::Hidden,
        };
        let ask =
            Ask { allow: label(&["allow_once"]), deny: label(&["reject_once", "reject_always"]), ..Ask::default() };
        node = node
            .confirm("card")
            .ask(ask.always(always))
            .on_action("allow", Msg::Answer(Approval::Once))
            .on_action("always", Msg::Answer(Approval::Always))
            .on_action("deny", Msg::Answer(Approval::Denied));
    }
    for (i, content) in tool.content.iter().enumerate() {
        let child: Node<Msg> = match content {
            ToolContent::Text(text) if tool.kind == "execute" => {
                ui::ansi(text.trim_matches('`').trim().to_string()).key(i).into()
            }
            ToolContent::Text(text) => ui::md(text.clone()).key(i).into(),
            ToolContent::Diff { path, old, new } => {
                ui::diff().key(i).path(path.clone()).prop("hunks", hunks(old.as_deref(), new)).into()
            }
        };
        node = node.child(child);
    }
    // A command ends with how it ended, as omp draws it.
    if tool.kind == "execute" && tool.status.settled() {
        let (icon, word, tone) = match tool.status {
            ToolStatus::Failed => ("x", "Failed", "error"),
            ToolStatus::Cancelled => ("slash-circle", "Stopped", "warning"),
            _ => ("check", "Done", "success"),
        };
        let mut footer = ui::row()
            .key("status")
            .role("omp.run.status")
            .gap("sm")
            .align("center")
            .child(ui::icon(icon).tone(tone).key("icon"))
            .child(ui::text(word).tone(tone).key("word"));
        if let Some(end) = tool.finished {
            let took = duration(end.saturating_duration_since(tool.started));
            footer = footer.child(ui::text(spans(&[(&took, "dim mono")])).key("took"));
        }
        node = node.child(footer);
    }
    node.into()
}

/// One hunk around the lines that changed between `old` and `new`, with a
/// few lines of context; a new file is all additions.
fn hunks(old: Option<&str>, new: &str) -> Value {
    let new_lines: Vec<&str> = new.lines().collect();
    let Some(old) = old else {
        let lines: Vec<String> = new_lines.iter().map(|l| format!("+{l}")).collect();
        return json!([{"oldStart": 0, "newStart": 1, "lines": lines}]);
    };
    let old_lines: Vec<&str> = old.lines().collect();
    let prefix = old_lines.iter().zip(&new_lines).take_while(|(a, b)| a == b).count();
    let max_suffix = old_lines.len().min(new_lines.len()) - prefix;
    let suffix =
        old_lines.iter().rev().zip(new_lines.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    let start = prefix.saturating_sub(CONTEXT);
    let mut lines: Vec<String> = old_lines[start..prefix].iter().map(|l| format!(" {l}")).collect();
    lines.extend(old_lines[prefix..old_lines.len() - suffix].iter().map(|l| format!("-{l}")));
    lines.extend(new_lines[prefix..new_lines.len() - suffix].iter().map(|l| format!("+{l}")));
    let tail = new_lines.len() - suffix;
    lines.extend(new_lines[tail..(tail + CONTEXT).min(new_lines.len())].iter().map(|l| format!(" {l}")));
    json!([{"oldStart": start + 1, "newStart": start + 1, "lines": lines}])
}

fn dock(t: &Transcript, composer: &Composer, info: &Info) -> Vec<Node<Msg>> {
    let mut nodes = Vec::new();
    if let Some(question) = &t.question {
        nodes.push(question_card(t, question));
    }
    if !t.plan.is_empty() {
        nodes.push(plan(t));
    }
    if !t.notes.is_empty() {
        nodes.push(notes(t));
    }
    if !t.queue.is_empty() {
        nodes.push(queue(t));
    }
    if let Some(working) = working(t, info) {
        nodes.push(working);
    }
    nodes.push(composer_node(t, composer, info));
    nodes
}

/// What happened while the pane was hidden, until the next message or a dismissal.
fn notes(t: &Transcript) -> Node<Msg> {
    let rows = t.notes.iter().enumerate().map(|(i, note)| {
        let icon = match note.tone {
            "success" => "check",
            "error" => "x-circle",
            _ => "bell",
        };
        ui::row()
            .key(i)
            .gap("xs")
            .align("center")
            .child(ui::icon(icon).tone(note.tone).key("i"))
            .child(ui::text(note.text.clone()).key("t"))
    });
    ui::row()
        .key("notes")
        .role("sand.notes")
        .align("center")
        .gap("md")
        .child(ui::col().key("list").gap("xs").grow(1.0).children(rows))
        .child(ui::button("Dismiss").key("dismiss").variant("quiet").size("sm").on_click(Msg::DismissNotes))
        .into()
}

/// The follow-ups waiting for the agent, each with its ✕; paused, a choice.
fn queue(t: &Transcript) -> Node<Msg> {
    let total = t.queue.len();
    let mut col = ui::col().key("queue").role("sand.queue").gap("xs");
    for (i, text) in t.queue.iter().enumerate() {
        let line: String = text.lines().next().unwrap_or_default().chars().take(120).collect();
        col = col.child(
            ui::row()
                .key(i)
                .role("sand.queued")
                .gap("sm")
                .align("center")
                .child(ui::text(spans(&[("queued · ", "muted"), (&line, "")])).key("t").grow(1.0))
                .child(ui::text(spans(&[(&format!("{}/{total}", i + 1), "muted mono")])).key("n"))
                .child(
                    ui::button("✕")
                        .key("drop")
                        .variant("quiet")
                        .size("sm")
                        .title("Take back")
                        .on_click(Msg::Unqueue(i)),
                ),
        );
    }
    if t.queue_paused {
        col = col.child(
            ui::row()
                .key("paused")
                .gap("sm")
                .align("center")
                .child(ui::text(spans(&[("Queue paused after the turn stopped", "warning")])).key("t").grow(1.0))
                .child(
                    ui::button("Continue queue").key("go").variant("primary").size("sm").on_click(Msg::QueueContinue),
                )
                .child(ui::button("Discard").key("drop").variant("quiet").size("sm").on_click(Msg::QueueDiscard)),
        );
    }
    col.into()
}

/// Setup's question, with what led to it and a button per answer.
fn question_card(t: &Transcript, question: &Question) -> Node<Msg> {
    let mut card = ui::card().key("question").role("agent.request").tone("warning").head(question.text.clone());
    let context: Vec<&str> = t
        .setup
        .steps
        .iter()
        .rev()
        .find(|s| s.status == StepStatus::Running)
        .map(|s| s.notes.iter().map(String::as_str).collect())
        .unwrap_or_default();
    if !context.is_empty() {
        card = card.child(ui::code(context.join("\n")).lang("text").key("context"));
    }
    let options = question.options.iter().enumerate().map(|(i, option)| {
        let (label, detail) = option.split_once(" — ").unwrap_or((option, ""));
        let button = ui::button(label).key("b").keys([(i + 1).to_string()]).on_click(Msg::Choose(i));
        let button = if i == 0 { button.variant("primary") } else { button };
        ui::row().key(i).gap("sm").align("center").child(button).child(ui::text(spans(&[(detail, "muted")])).key("d"))
    });
    card = card.child(ui::col().key("options").gap("xs").children(options));
    card.child(ui::text(spans(&[("1–9 picks · Enter takes the first · Esc aborts", "muted")])).key("hint")).into()
}

fn plan(t: &Transcript) -> Node<Msg> {
    let items = t
        .plan
        .iter()
        .enumerate()
        .map(|(i, entry)| ChecklistItem {
            id: format!("p{i}"),
            text: entry.content.clone().into(),
            status: Some(
                match entry.status.as_str() {
                    "completed" => "done",
                    "in_progress" => "active",
                    _ => "pending",
                }
                .into(),
            ),
            note: None,
        })
        .collect();
    ui::checklist().key("plan").mode("hud").phases(vec![Phase { id: "plan".into(), items, ..Phase::default() }]).into()
}

/// The live line above the composer, as omp's loader: spinner, elapsed, what is going on.
fn working(t: &Transcript, info: &Info) -> Option<Node<Msg>> {
    let (key, label) = match t.turn {
        _ if t.question.is_some() => return None,
        Turn::Setup => {
            let step = t.setup.steps.iter().rev().find(|s| s.status == StepStatus::Running);
            ("setup".to_string(), step.map_or_else(|| "Setting up".to_string(), |s| s.title.clone()))
        }
        Turn::Starting => match &t.resuming {
            Some(title) => ("resuming".to_string(), format!("Resuming {title}")),
            None => ("starting".to_string(), format!("Starting {}", info.product)),
        },
        Turn::Working if t.permission.is_none() => {
            let label = match (t.running_tool(), t.entries.last()) {
                (Some(tool), _) => match &tool.target {
                    Some((target, _)) => format!("{} {target}", verb(&tool.kind)),
                    None => tool.title.clone(),
                },
                (None, Some(Entry::Agent(_))) => "Writing…".to_string(),
                _ => "Thinking…".to_string(),
            };
            // A new elapsed clock per turn.
            let turn = t.entries.iter().filter(|e| matches!(e, Entry::User(_))).count();
            (format!("turn{turn}"), label)
        }
        _ => return None,
    };
    let label: String = label.chars().take(60).collect();
    Some(
        ui::row()
            .key("working")
            .role("omp.working")
            .align("center")
            .gap("sm")
            .child(ui::spinner().style("starburst").tone("accent").key("spinner"))
            .child(ui::elapsed().age(0i64).format("short").key(key))
            .child(ui::text("·").role("omp.working.sep").key("sep"))
            .child(ui::shimmer(label).role("omp.working.label").key("label"))
            .child(ui::row().grow(1.0).key("fill"))
            .into(),
    )
}

/// The floating composer: the context bar, the editor, and a bar of the
/// session's facts. Its roles are sand's own: Tern folds a composer with omp's
/// roles to one row whenever the pane is not typed in, and this one stays put
/// (`STYLE` draws it).
fn composer_node(t: &Transcript, composer: &Composer, info: &Info) -> Node<Msg> {
    let placeholder = match t.turn {
        _ if t.question.is_some() => "Answer the question above".to_string(),
        _ if !t.queue.is_empty() && t.turn != Turn::Idle => "Another follow-up? It waits its turn".to_string(),
        Turn::Setup | Turn::Starting => format!("Ask {} anything — it is sent once the agent is ready", info.product),
        Turn::Working if t.permission.is_some() => "Enter allows · Esc denies".to_string(),
        Turn::Working => format!("Esc stops {} · Enter queues a follow-up", info.product),
        Turn::Idle => format!("Ask {} anything — / for commands", info.product),
        Turn::Exited => "Enter closes".to_string(),
    };
    let editor = ui::editor()
        .key("input")
        .text(composer.draft.clone())
        .cursor(composer.cursor_utf16())
        .placeholder(placeholder)
        .sendable(t.turn == Turn::Idle)
        // Always a box of its own, however empty, at the bottom of the pane.
        .min(Bound::h(Extent::Lines(2.0)))
        .max_lines(12u32)
        .on_send(|text| Msg::Send(text.to_owned()))
        .on_edit(|e| Msg::Edit { from: e.from, to: e.to, text: e.text.clone(), cursor: e.cursor, len: e.len })
        .on_undo(Msg::Undo)
        .on_focus(Msg::Focus);
    // The rest of the highlighted command, faint after the caret.
    let completions = if composer.dismissed { Vec::new() } else { commands::complete(&composer.draft, &t.commands) };
    let editor = match completions.get(composer.pick.min(completions.len().saturating_sub(1))) {
        Some(command) if composer.at_end() => {
            let typed = composer.draft.trim_start_matches('/').len();
            editor.ghost(command.name.get(typed..).unwrap_or_default().to_string())
        }
        _ => editor,
    };
    let mut col = ui::col().key("composer").role("sand.composer");
    if let Some((used, size)) = t.context.filter(|(_, size)| *size > 0) {
        let fraction = (used as f64 / size as f64).min(1.0);
        col = col.child(
            ui::meter()
                .key("context")
                .role("sand.composer.context")
                .title(format!("{} of {} tokens of context", compact(used), compact(size)))
                .value(fraction)
                .thresholds(Thresholds { warn: Some(0.75), bad: Some(0.9) })
                .label(format!("{:.0}%", fraction * 100.0))
                .total(compact(size)),
        );
    }
    col.child(editor).child(bar(t, info)).child(facts(t, info)).into()
}

/// The model or effort: a picker of the agent's choices once it offers
/// them, else what the host's settings name.
fn chip(t: &Transcript, key: &str, role: &str, icon: &str, text: &str, label: &str, setting: Setting) -> Node<Msg> {
    let (choices, current) = t.choices(setting);
    // A mode that drops the agent's permission prompts reads as a warning.
    let tone = match (setting, current.as_deref()) {
        (Setting::Mode, Some(mode)) if mode != "default" => "warning",
        _ => "accent",
    };
    if choices.len() > 1 {
        let options: Vec<ui::SelectOption> = choices
            .into_iter()
            .map(|(value, name)| ui::SelectOption { icon: Some(icon.into()), ..ui::SelectOption::new(value, name) })
            .collect();
        let hint = if setting == Setting::Mode { " · Shift+Tab cycles" } else { "" };
        let select = ui::select()
            .key(key)
            .role(role)
            .variant("quiet")
            .label(label)
            .size("sm")
            .options(options)
            .value(current.unwrap_or_default())
            .title(format!("{label} · click to change{hint}"))
            .on_change(move |change| Msg::Pick(setting, change.value.as_str().unwrap_or_default().to_string()));
        return if tone == "warning" { select.tone(tone).into() } else { select.into() };
    }
    ui::row()
        .key(key)
        .role(role)
        .gap("xs")
        .align("center")
        .title(label.to_string())
        .child(ui::icon(icon).tone(tone).key("i"))
        .child(ui::text(text.to_string()).key("t"))
        .into()
}

/// One fact of the status line: an icon and its text, both in `tone`.
fn fact(key: &str, icon: &str, text: String, tone: &str, title: String) -> Node<Msg> {
    let icon = ui::icon(icon).key("i");
    let text = ui::text(text).key("t");
    let (icon, text) = if tone.is_empty() { (icon, text) } else { (icon.tone(tone), text.tone(tone)) };
    ui::row().key(key).gap("xs").align("center").title(title).child(icon).child(text).into()
}

/// The status line's facts about where the session runs, on a line of their
/// own under the bar: they wrap rather than give way, however long the chips.
fn facts(t: &Transcript, info: &Info) -> Node<Msg> {
    let mut facts = Vec::new();
    let state = match t.turn {
        Turn::Setup if t.setup.failed() => Some(("setup failed", "error", "x-circle")),
        Turn::Setup => Some(("setting up", "pending", "box")),
        Turn::Starting => Some(("starting", "pending", "play")),
        Turn::Exited => Some(("exited", "error", "x-circle")),
        _ => None,
    };
    if let Some((text, tone, icon)) = state {
        facts.push(fact("state", icon, text.to_string(), tone, "Session".to_string()));
    }
    // Where it runs: the sandbox and its branch, kept together.
    let mut place = ui::row().key("place").gap("sm").align("center").child(fact(
        "sandbox",
        "box",
        info.sandbox.clone(),
        "accent",
        "Sandbox".to_string(),
    ));
    if let Some(branch) = &info.branch {
        place = place.child(fact("branch", "branch", branch.clone(), "", "Git branch".to_string()));
    }
    facts.push(place.into());
    let api = match info.api {
        Some(ApiState::Ok) => Some(("API ✓", "success")),
        Some(ApiState::Blocked) => Some(("API blocked", "error")),
        Some(ApiState::Unreachable) => Some(("API ✗", "error")),
        _ => None,
    };
    if let Some((text, tone)) = api {
        let title = info.api_host.clone().unwrap_or_else(|| "The agent's API".to_string());
        facts.push(fact("api", "globe", text.to_string(), tone, title));
    }
    facts.push(mcp_menu(&info.mcp));
    ui::row().key("facts").role("sand.composer.facts").align("center").wrap(true).children(facts).into()
}

/// The MCP servers: their worst state on the trigger (green when all are
/// fine, a warning when one needs attention, failed when one failed), and
/// each server's state in its menu.
fn mcp_menu(mcp: &[McpStatus]) -> Node<Msg> {
    if mcp.is_empty() {
        return fact("mcp", "plug", "MCP none".to_string(), "muted", "No MCP servers".to_string());
    }
    let worst = |state: McpState| match state {
        McpState::Ok => 0,
        McpState::Unknown => 1,
        McpState::NeedsAuth => 2,
        McpState::Error => 3,
    };
    let state = mcp.iter().map(|s| s.state).max_by_key(|s| worst(*s)).unwrap_or(McpState::Unknown);
    let (icon, tone, summary) = match state {
        McpState::Ok => ("check", "success", "all connected"),
        McpState::Unknown => ("help", "muted", "not checked yet"),
        McpState::NeedsAuth => ("warn", "warning", "one needs attention"),
        McpState::Error => ("x-circle", "error", "one failed"),
    };
    let mut items = vec![json!({"header": "MCP servers"})];
    items.extend(mcp.iter().map(|server| {
        let (icon, detail) = match server.state {
            McpState::Ok => ("check", "connected".to_string()),
            McpState::NeedsAuth => ("warn", format!("needs authorization: sbx mcp auth {}", server.name)),
            McpState::Error => ("x-circle", "failed".to_string()),
            McpState::Unknown => ("help", "not checked yet".to_string()),
        };
        let mut item =
            json!({"id": format!("mcp.{}", server.name), "label": server.name, "icon": icon, "detail": detail});
        if server.state == McpState::Error {
            item["danger"] = json!(true);
        }
        item
    }));
    ui::node("menu")
        .key("mcp")
        .role("sand.composer.mcp")
        .tone(tone)
        .title(format!("MCP servers: {summary}"))
        .prop("text", format!("MCP {}", mcp.len()))
        .prop("icon", icon)
        .prop("trigger", "quiet")
        .prop("size", "sm")
        .prop("caret", true)
        .prop("items", items)
}

/// The composer's bar: the model, effort and mode pickers, the cost and send.
fn bar(t: &Transcript, info: &Info) -> Node<Msg> {
    let mut row = ui::row().key("bar").role("sand.composer.bar").align("center");
    row = row.child(chip(t, "model", "sand.composer.model", "sparkle", info.model(t), "Model", Setting::Model));
    if let Some(effort) = info.effort(t) {
        row = row.child(chip(t, "effort", "sand.composer.effort", "brain", effort, "Effort", Setting::Effort));
    }
    if let Some(mode) = t.mode_name() {
        row = row.child(chip(t, "mode", "sand.composer.mode", "shield", mode, "Mode", Setting::Mode));
    }

    // The chips take what they need; cost and send sit at the right.
    row = row.child(ui::row().key("fill").grow(1.0));

    if let Some(usd) = t.cost {
        row = row.child(
            ui::text(spans(&[(&cost(usd), "muted")])).key("usage").role("sand.composer.usage").title("Session cost"),
        );
    }
    let action = if t.turn == Turn::Working {
        ui::row()
            .key("stop")
            .role("sand.composer.stop")
            .title("Stop (Esc)")
            .gap("xs")
            .align("center")
            .child(ui::text("Stop").key("t"))
            .on_click(Msg::Stop)
    } else {
        ui::row()
            .key("send")
            .role("sand.composer.send")
            .title("Send")
            .gap("xs")
            .align("center")
            .child(ui::kbd(["enter"]).key("k"))
            .on_click(Msg::Submit)
    };
    row.child(action).into()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::log::Line;

    fn info() -> Info {
        Info {
            harness: "claude".into(),
            product: "Claude Code".into(),
            sandbox: "claude-app".into(),
            workspace: Some("~/src/app".into()),
            mounts: vec!["/src/app (rw)  [primary workspace]".into()],
            mcp: vec![McpStatus { name: "github".into(), state: McpState::NeedsAuth }],
            api: Some(ApiState::Ok),
            api_host: Some("api.anthropic.com".into()),
            branch: Some("main".into()),
            model: Some("opus".into()),
            effort: Some("high".into()),
        }
    }

    fn region(view: &View<Msg>, index: usize) -> &[Node<Msg>] {
        view.regions()[index].expect("region").child_nodes()
    }

    fn find<'a>(nodes: &'a [Node<Msg>], role: &str) -> Option<&'a Node<Msg>> {
        nodes.iter().find_map(|n| {
            if n.props().get("role").and_then(Value::as_str) == Some(role) {
                Some(n)
            } else {
                find(n.child_nodes(), role)
            }
        })
    }

    #[test]
    fn setup_shows_as_steps_then_folds() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.setup.log(Line::Step("Starting the sandbox".into()), now);
        t.setup.log(Line::Wait("waiting for the sandbox".into()), now);
        let v = view(&t, &Composer::default(), &info());
        let setup = &region(&v, 0)[0];
        assert_eq!((setup.kind(), setup.props()["variant"].as_str()), ("trace", Some("rail")));
        assert_eq!(setup.props()["lines"][0]["detail"], "waiting for the sandbox");
        assert_eq!(setup.props()["lines"][0]["status"], "running");
        let working = find(region(&v, 1), "omp.working").expect("a working line");
        assert_eq!(find(working.child_nodes(), "omp.working.label").unwrap().props()["text"], "Starting the sandbox");
        let facts = find(region(&v, 1), "sand.composer.facts").unwrap();
        assert_eq!(facts.child_nodes()[0].child_nodes()[1].props()["text"], "setting up");

        t.setup.finish(now + Duration::from_secs(12));
        t.turn = Turn::Idle;
        let v = view(&t, &Composer::default(), &info());
        let main = region(&v, 0);
        assert_eq!(main[0].props()["variant"], "summary");
        assert_eq!(main[0].props()["summary"], "Set up claude-app in 12.0s · 1 steps");
        assert_eq!(main[1].props()["role"], "omp.welcome");
    }

    #[test]
    fn setup_questions_offer_a_button_per_answer() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.setup.log(Line::Step("Sandbox 'x' exists with a different setup".into()), now);
        t.setup.log(Line::Info("mounts: a, b".into()), now);
        t.question = Some(Question {
            text: "How do you want to continue?".into(),
            options: vec!["Replace — make it match".into(), "Abort — leave it".into()],
        });
        let v = view(&t, &Composer::default(), &info());
        let card = &region(&v, 1)[0];
        assert_eq!(
            (card.props()["role"].as_str(), card.props()["tone"].as_str()),
            (Some("agent.request"), Some("warning"))
        );
        assert_eq!(card.child_nodes()[0].props()["text"], "mounts: a, b");
        let first = &card.child_nodes()[1].child_nodes()[0];
        assert_eq!(first.child_nodes()[0].props()["text"], "Replace");
        assert_eq!(first.child_nodes()[0].props()["variant"], "primary");
        assert_eq!(first.child_nodes()[1].props()["spans"][0]["t"], "make it match");
        assert!(find(region(&v, 1), "omp.working").is_none());
    }

    #[test]
    fn a_turn_is_grouped_with_its_tools_and_usage() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.turn = Turn::Idle;
        t.prompt("fix it", now);
        t.apply(
            &json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "`cargo test`", "kind": "execute",
                "status": "completed", "rawInput": {"command": "cargo test"},
                "content": [{"type": "content", "content": {"type": "text", "text": "```\nok\n```"}}]}),
            now,
        );
        t.apply(
            &json!({"sessionUpdate": "tool_call", "toolCallId": "t2", "kind": "edit", "status": "completed",
                "locations": [{"path": "/w/a.rs"}],
                "content": [{"type": "diff", "path": "/w/a.rs", "oldText": "a\nb", "newText": "a\nc\nd"}]}),
            now,
        );
        t.apply(&json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}), now);
        t.apply(
            &json!({"sessionUpdate": "usage_update", "used": 80000, "size": 200000, "cost": {"amount": 0.04}}),
            now,
        );
        t.turn_ended(Some("end_turn"), &json!({"totalTokens": 18000}), now + Duration::from_secs(8));
        let v = view(&t, &Composer::default(), &info());
        let main = region(&v, 0);
        assert_eq!(main[0].props()["role"], "omp.user");
        let turn = &main[1];
        assert_eq!(turn.props()["role"], "omp.assistant");
        let kinds: Vec<&str> = turn.child_nodes().iter().map(Node::kind).collect();
        assert_eq!(kinds, ["tool", "tool", "md", "row"]);
        let bash = &turn.child_nodes()[0];
        assert_eq!((bash.props()["title"].as_str(), bash.props()["frame"].as_str()), (Some("Bash"), Some("inline")));
        assert_eq!(bash.child_nodes()[0].props()["text"], "ok");
        assert_eq!(bash.child_nodes()[1].props()["role"], "omp.run.status");
        let edit = &turn.child_nodes()[1];
        assert_eq!(edit.props()["meta"], json!(["+2 −1"]));
        assert_eq!(edit.props()["collapsed"], false);
        assert_eq!(
            find(turn.child_nodes(), "omp.turn.usage").unwrap().child_nodes()[0].props()["spans"][0]["t"],
            "8.0s · 18,000 tokens · $0.04 this turn"
        );

        let dock = region(&v, 1);
        let context = find(dock, "sand.composer.context").expect("a context bar");
        assert_eq!((context.props()["value"].as_f64(), context.props()["label"].as_str()), (Some(0.4), Some("40%")));
        let facts = find(dock, "sand.composer.facts").unwrap();
        assert_eq!(facts.props()["wrap"], true);
        let place = &facts.child_nodes()[0];
        let texts: Vec<&str> =
            place.child_nodes().iter().map(|f| f.child_nodes()[1].props()["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["claude-app", "main"], "the sandbox and its branch, on one line");
        assert_eq!(facts.child_nodes()[1].child_nodes()[1].props()["tone"], "success");
        let mcp = facts.child_nodes()[2].props();
        assert_eq!(facts.child_nodes()[2].kind(), "menu");
        assert_eq!(
            (mcp["text"].as_str(), mcp["icon"].as_str(), mcp["tone"].as_str()),
            (Some("MCP 1"), Some("warn"), Some("warning"))
        );
        assert_eq!(mcp["items"][1]["label"], "github");
        assert_eq!(mcp["items"][1]["detail"], "needs authorization: sbx mcp auth github");
        assert_eq!(find(dock, "sand.composer.usage").unwrap().props()["spans"][0]["t"], "$0.04");
        assert_eq!(main.last().unwrap().props()["role"], "sand.tail");
        assert_eq!(main.last().unwrap().kind(), "spacer");
    }

    #[test]
    fn the_composer_always_shows_its_box_and_the_model_with_its_effort() {
        let mut t = Transcript::default();
        // Before the agent says, the host's settings name them; nothing can be changed yet.
        let v = view(&t, &Composer::default(), &info());
        let composer = find(region(&v, 1), "sand.composer").unwrap();
        assert_eq!(composer.child_nodes()[0].props()["min"], json!({"h": "2lines"}));
        let text = |role: &str, v: &View<Msg>| {
            let chip = find(region(v, 1), role).unwrap();
            (chip.child_nodes()[1].props()["text"].as_str().unwrap().to_string(), chip.child_nodes().len())
        };
        assert_eq!(text("sand.composer.model", &v), ("opus".into(), 2));
        assert_eq!(text("sand.composer.effort", &v), ("high".into(), 2));

        t.turn = Turn::Exited;
        t.set_options(&json!([
            {"id": "model", "category": "model", "currentValue": "sonnet",
             "options": [{"value": "sonnet", "name": "Sonnet 5.5"}, {"value": "opus", "name": "Opus 5.5"}]},
            {"id": "effort", "category": "thought_level", "currentValue": "low",
             "options": [{"value": "low", "name": "low"}, {"value": "max", "name": "max"}]}]));
        let v = view(&t, &Composer::default(), &info());
        let picker = |role: &str| find(region(&v, 1), role).unwrap().props().clone();
        let model = picker("sand.composer.model");
        assert_eq!((model["value"].as_str(), model["variant"].as_str()), (Some("sonnet"), Some("quiet")));
        assert_eq!(model["options"][1], json!({"value": "opus", "label": "Opus 5.5", "icon": "sparkle"}));
        assert_eq!(picker("sand.composer.effort")["options"].as_array().map(Vec::len), Some(2));
        let composer = find(region(&v, 1), "sand.composer").unwrap();
        assert!(composer.child_nodes()[0].props().get("readonly").is_none());

        // The mode is a picker too, warning-toned when it drops the permission prompts.
        t.modes = vec![
            super::super::model::Mode { id: "default".into(), name: "Default".into() },
            super::super::model::Mode { id: "bypassPermissions".into(), name: "Bypass Permissions".into() },
        ];
        t.mode = Some("bypassPermissions".into());
        let v = view(&t, &Composer::default(), &info());
        let mode = find(region(&v, 1), "sand.composer.mode").unwrap().props().clone();
        assert_eq!(mode["value"], "bypassPermissions");
        assert_eq!((mode["variant"].as_str(), mode["tone"].as_str()), (Some("quiet"), Some("warning")));
        assert_eq!(mode["options"][1]["label"], "Bypass Permissions");
    }

    #[test]
    fn a_tool_waiting_for_permission_asks_with_the_agents_labels() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.turn = Turn::Working;
        let params = json!({"toolCall": {"toolCallId": "t1", "title": "Write a.rs", "kind": "edit",
                "locations": [{"path": "/w/a.rs"}]},
            "options": [{"optionId": "y", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "n", "name": "Reject", "kind": "reject_once"}]});
        t.ask(json!(1), &params, now);
        let v = view(&t, &Composer::default(), &info());
        let tool = &region(&v, 0)[0].child_nodes()[0];
        assert_eq!((tool.props()["status"].as_str(), tool.props()["frame"].as_str()), (Some("ask"), Some("card")));
        assert_eq!(tool.props()["ask"], json!({"allow": "Allow", "deny": "Reject", "always": false}));
        let dock = region(&v, 1);
        assert!(find(dock, "omp.working").is_none());
        let composer = find(dock, "sand.composer").unwrap();
        assert_eq!(composer.child_nodes()[0].props()["placeholder"], "Enter allows · Esc denies");
    }

    #[test]
    fn slash_commands_complete_under_the_caret_and_sessions_pick() {
        let mut t = Transcript::default();
        t.turn = Turn::Idle;
        t.commands = vec![commands::SlashCommand {
            name: "review".into(),
            description: "Review a PR".into(),
            hint: Some("PR number".into()),
            builtin: false,
        }];
        let mut composer = Composer::default();
        composer.insert("/re");
        composer.pick = 1;
        let v = view(&t, &composer, &info());
        let overlay = &region(&v, 2)[0];
        assert_eq!(overlay.props()["role"], "omp.autocomplete");
        assert_eq!(overlay.props()["anchor"], json!({"caret": EDITOR}));
        let list = &overlay.child_nodes()[0];
        assert_eq!(list.props()["selected"], "layer.complete.list.c1");
        let labels: Vec<&str> = list.child_nodes().iter().map(|i| i.props()["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["/resume", "/review"]);
        assert_eq!(list.child_nodes()[1].props()["detail"], "Review a PR");
        assert_eq!(list.child_nodes()[1].props()["value"], "PR number");
        composer.dismissed = true;
        assert!(region(&view(&t, &composer, &info()), 2).is_empty());

        t.picker = Some(SessionPicker {
            sessions: vec![super::super::model::SessionEntry {
                id: "abcdef123456".into(),
                title: Some("Fix the build".into()),
                cwd: Some("/w".into()),
                updated: Some("2026-11-02T10:00:00Z".into()),
            }],
            ..SessionPicker::default()
        });
        let v = view(&t, &Composer::default(), &info());
        let picker = &region(&v, 2)[0];
        assert_eq!(picker.props()["role"], "sand.sessions");
        let item = &picker.child_nodes().iter().find(|n| n.kind() == "list").unwrap().child_nodes()[0];
        assert_eq!(item.props()["detail"], "2026-11-02 10:00 · /w");
        assert_eq!(item.props()["value"], "abcdef12");
        assert_eq!(item_index("layer.sessions.list.s12"), 12);
    }

    #[test]
    fn the_composer_shows_the_rest_of_a_command_and_whats_waiting() {
        let mut t = Transcript::default();
        t.turn = Turn::Working;
        let mut composer = Composer::default();
        composer.insert("/ses");
        let v = view(&t, &composer, &info());
        let editor = &find(region(&v, 1), "sand.composer").unwrap().child_nodes()[0];
        assert_eq!(editor.props()["ghost"], "sions");

        t.queue = vec!["then the docs".into(), "and the changelog".into()];
        t.queue_paused = true;
        t.hidden = true;
        t.note("Claude Code finished", "success");
        let v = view(&t, &Composer::default(), &info());
        let dock = region(&v, 1);
        let notes = find(dock, "sand.notes").expect("notes");
        assert_eq!(notes.child_nodes()[0].child_nodes()[0].child_nodes()[1].props()["text"], "Claude Code finished");
        let queue = find(dock, "sand.queue").expect("the queue");
        assert_eq!(queue.child_nodes().len(), 3, "two follow-ups and the paused choice");
        assert_eq!(queue.child_nodes()[0].child_nodes()[0].props()["spans"][1]["t"], "then the docs");
        assert_eq!(queue.child_nodes()[2].child_nodes()[1].props()["text"], "Continue queue");
    }

    #[test]
    fn the_welcome_offers_the_last_session_and_starters() {
        let mut t = Transcript::default();
        t.turn = Turn::Idle;
        t.last_session = Some(super::super::model::SessionEntry {
            id: "S9".into(),
            title: Some("Fix the build".into()),
            cwd: None,
            updated: None,
        });
        let v = view(&t, &Composer::default(), &info());
        let starters = find(region(&v, 0), "agent.starters").unwrap();
        let labels: Vec<&str> = starters.child_nodes().iter().map(|b| b.props()["text"].as_str().unwrap()).collect();
        assert_eq!(labels[0], "Continue “Fix the build”");
        assert_eq!(labels.len(), 1 + SUGGESTIONS.len());
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn the_mcp_menu_shows_the_worst_state() {
        let server = |name: &str, state| McpStatus { name: name.into(), state };
        let ok = mcp_menu(&[server("a", McpState::Ok), server("b", McpState::Ok)]);
        assert_eq!((ok.props()["icon"].as_str(), ok.props()["tone"].as_str()), (Some("check"), Some("success")));
        let failed = mcp_menu(&[server("a", McpState::NeedsAuth), server("b", McpState::Error)]);
        assert_eq!(
            (failed.props()["icon"].as_str(), failed.props()["tone"].as_str()),
            (Some("x-circle"), Some("error"))
        );
        assert_eq!(failed.props()["items"][2]["danger"], true);
        assert_eq!(mcp_menu(&[]).kind(), "row");
    }

    #[test]
    fn diffs_show_the_change_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh";
        let new = "a\nb\nc\nd\nE\nf\ng\nh";
        assert_eq!(
            hunks(Some(old), new),
            json!([{"oldStart": 2, "newStart": 2, "lines": [" b", " c", " d", "-e", "+E", " f", " g", " h"]}])
        );
        assert_eq!(hunks(None, "x\ny"), json!([{"oldStart": 0, "newStart": 1, "lines": ["+x", "+y"]}]));
        assert_eq!(hunks(Some("same"), "same"), json!([{"oldStart": 1, "newStart": 1, "lines": [" same"]}]));
    }

    #[test]
    fn numbers_read_short() {
        assert_eq!(
            (compact(18_000), compact(200_000), compact(1_234_567), compact(950)),
            ("18k".into(), "200k".into(), "1.2M".into(), "950".into())
        );
        assert_eq!(
            (duration(Duration::from_millis(120)), duration(Duration::from_secs(75))),
            ("120ms".into(), "1m 15s".into())
        );
        assert_eq!((cost(0.001), cost(0.256)), ("<$0.01".into(), "$0.26".into()));
    }
}
