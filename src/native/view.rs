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
    self, Ask, AskAlways, ChecklistItem, Node, Phase, Span, Thresholds, ToolButton, ToolPreview, TraceLine, View,
};

use super::composer::Composer;
use super::model::{
    Approval, Entry, Permission, Question, Setup, StepStatus, TargetKind, ToolCall, ToolContent, ToolStatus,
    Transcript, Turn, TurnUsage,
};
use crate::mcp::{McpState, McpStatus};
use crate::runtime::ApiState;

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
    /// The mode segment.
    CycleMode,
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
}

pub fn view(t: &Transcript, composer: &Composer, info: &Info) -> View<Msg> {
    View::new().main(ui::col().children(transcript(t, info))).dock(ui::col().children(dock(t, composer, info)))
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
            nodes.push(user(key, text, false));
            continue;
        }
        if turn_key.is_empty() {
            turn_key = format!("turn{i}");
        }
        let streaming = i == last && t.turn == Turn::Working;
        turn.push(match entry {
            Entry::User(_) => unreachable!("handled above"),
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
    if let Some(text) = &t.queued {
        nodes.push(user("queued".to_string(), text, true));
    }
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
    let mut where_ = vec![Span::new(t.model.clone().unwrap_or_else(|| info.product.clone())).style("accent")];
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
    let tips: Vec<Span> = [("⇧Enter", "new line"), ("⇧Tab", "mode"), ("Esc", "stop"), ("Ctrl+D", "quit")]
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
        .child(ui::text(tips).key("tips").role("omp.welcome.tip"))
        .into()
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
fn user(key: String, text: &str, queued: bool) -> Node<Msg> {
    let mut card = ui::card().key(key).role("omp.user");
    if queued {
        card = card.child(
            ui::row()
                .key("badges")
                .role("omp.user.badges")
                .justify("end")
                .child(ui::badge("queued · sent once the agent is ready").tone("accent").key("queued")),
        );
    }
    card.child(ui::md(text.to_string()).key("text")).into()
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
    parts.extend(usage.tokens.map(|n| format!("{} tok", compact(n))));
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
    if let Some(working) = working(t, info) {
        nodes.push(working);
    }
    nodes.push(composer_node(t, composer, info));
    nodes
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
        Turn::Starting => ("starting".to_string(), format!("Starting {}", info.product)),
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

/// The floating composer: the context bar, the editor, and a bar of the session's facts.
fn composer_node(t: &Transcript, composer: &Composer, info: &Info) -> Node<Msg> {
    let placeholder = match t.turn {
        _ if t.question.is_some() => "Answer the question above".to_string(),
        Turn::Setup | Turn::Starting if t.queued.is_some() => format!("Setting up {}…", info.sandbox),
        Turn::Setup | Turn::Starting => format!("Ask {} anything — it is sent once the agent is ready", info.product),
        Turn::Working if t.permission.is_some() => "Enter allows · Esc denies".to_string(),
        Turn::Working => format!("Esc stops {}", info.product),
        Turn::Idle => format!("Ask {} anything", info.product),
        Turn::Exited => "Enter closes".to_string(),
    };
    let editor = ui::editor()
        .key("input")
        .text(composer.draft.clone())
        .cursor(composer.cursor_utf16())
        .placeholder(placeholder)
        .sendable(t.turn == Turn::Idle)
        .readonly(t.turn == Turn::Exited)
        .max_lines(12u32)
        .on_send(|text| Msg::Send(text.to_owned()))
        .on_focus(Msg::Focus);
    let mut col = ui::col().key("composer").role("omp.editor");
    if let Some((used, size)) = t.context.filter(|(_, size)| *size > 0) {
        let fraction = (used as f64 / size as f64).min(1.0);
        col = col.child(
            ui::meter()
                .key("context")
                .role("omp.composer.context")
                .title(format!("{} of {} tokens of context", compact(used), compact(size)))
                .value(fraction)
                .thresholds(Thresholds { warn: Some(0.75), bad: Some(0.9) })
                .label(format!("{:.0}%", fraction * 100.0))
                .total(compact(size)),
        );
    }
    col.child(editor).child(bar(t, info)).into()
}

/// The status line's facts, in the composer's bar.
fn bar(t: &Transcript, info: &Info) -> Node<Msg> {
    let model = t.model.clone().unwrap_or_else(|| info.product.clone());
    let mut row = ui::row().key("bar").role("omp.composer.bar").align("center").child(
        ui::row()
            .key("model")
            .role("omp.composer.model")
            .gap("xs")
            .align("center")
            .title("Model")
            .child(ui::icon("sparkle").tone("accent").key("i"))
            .child(ui::text(model).key("t")),
    );

    let mut status = ui::status().key("extras").role("omp.composer.extras").transparent(true).grow(1.0);
    let state = match t.turn {
        Turn::Setup if t.setup.failed() => Some(("setup failed", "error", "x-circle")),
        Turn::Setup => Some(("setting up", "pending", "box")),
        Turn::Starting => Some(("starting", "pending", "play")),
        Turn::Exited => Some(("exited", "error", "x-circle")),
        _ => None,
    };
    if let Some((text, tone, icon)) = state {
        status = status.child(ui::seg(text).key("state").icon(icon).tone(tone).priority(10.0));
    }
    status = status
        .child(ui::seg(info.sandbox.clone()).key("sandbox").icon("box").tone("accent").priority(9.0).title("Sandbox"));
    if let Some(branch) = &info.branch {
        status = status.child(ui::seg(branch.clone()).key("branch").icon("branch").priority(3.0).title("Git branch"));
    }
    let api = match info.api {
        Some(ApiState::Ok) => Some(("API ✓", "success")),
        Some(ApiState::Blocked) => Some(("API blocked", "error")),
        Some(ApiState::Unreachable) => Some(("API ✗", "error")),
        _ => None,
    };
    if let Some((text, tone)) = api {
        let title = info.api_host.clone().unwrap_or_else(|| "The agent's API".to_string());
        status = status.child(ui::seg(text).key("api").icon("globe").tone(tone).priority(6.0).title(title));
    }
    if info.mcp.is_empty() {
        status = status.child(ui::seg("MCP none").key("mcp").icon("plug").tone("muted").priority(2.0));
    }
    for (i, server) in info.mcp.iter().enumerate() {
        let (mark, tone) = mcp_mark(server.state);
        status = status.child(
            ui::seg(format!("{} {mark}", server.name))
                .key(format!("mcp{i}"))
                .icon("plug")
                .tone(tone)
                .priority(4.0)
                .title(format!("MCP server {}", server.name)),
        );
    }
    if let Some(mode) = t.mode_name() {
        let tone = if t.mode.as_deref() == Some("default") { "neutral" } else { "warning" };
        let mut seg = ui::seg(mode.to_string()).key("mode").icon("shield").tone(tone).priority(7.0);
        if t.modes.len() > 1 {
            seg = seg.title("Mode · Shift+Tab or click to change").on_click(Msg::CycleMode);
        }
        status = status.child(seg);
    }
    row = row.child(status);

    if let Some(usd) = t.cost {
        row = row.child(
            ui::text(spans(&[(&cost(usd), "muted")])).key("usage").role("omp.composer.usage").title("Session cost"),
        );
    }
    let action = if t.turn == Turn::Working {
        ui::row()
            .key("stop")
            .role("omp.composer.stop")
            .title("Stop (Esc)")
            .gap("xs")
            .align("center")
            .child(ui::text("Stop").key("t"))
            .on_click(Msg::Stop)
    } else {
        ui::row()
            .key("send")
            .role("omp.composer.send")
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
        assert_eq!(find(region(&v, 1), "omp.composer.extras").unwrap().child_nodes()[0].props()["text"], "setting up");

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
            "8.0s · 18k tok · $0.04 this turn"
        );

        let dock = region(&v, 1);
        let context = find(dock, "omp.composer.context").expect("a context bar");
        assert_eq!((context.props()["value"].as_f64(), context.props()["label"].as_str()), (Some(0.4), Some("40%")));
        let extras = find(dock, "omp.composer.extras").unwrap();
        let segs: Vec<(&str, &str)> = extras
            .child_nodes()
            .iter()
            .map(|s| (s.props()["text"].as_str().unwrap(), s.props().get("tone").and_then(Value::as_str).unwrap_or("")))
            .collect();
        assert_eq!(segs, [("claude-app", "accent"), ("main", ""), ("API ✓", "success"), ("github ⚠ auth", "warning")]);
        assert_eq!(find(dock, "omp.composer.usage").unwrap().props()["spans"][0]["t"], "$0.04");
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
        let composer = find(dock, "omp.editor").unwrap();
        assert_eq!(composer.child_nodes()[0].props()["placeholder"], "Enter allows · Esc denies");
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
