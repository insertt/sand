//! Draws the session from Tern's agent and chat kinds, the way Tern's own ACP
//! chat and omp do: the transcript as `message`s with `tool` cards and
//! `trace` thoughts in `main`; the plan, the composer and a status bar in the
//! dock. A function of the state alone: the SDK sends only what changed.

use serde_json::{Value, json};
use tern_sdk::ui::{self, Ask, AskAlways, ChecklistItem, Node, Phase, Span, ToolPreview, View};

use super::composer::Composer;
use super::model::{Approval, Entry, Permission, TargetKind, ToolCall, ToolContent, ToolStatus, Transcript, Turn};

/// The composer's id: region `dock`, key `ed`.
pub const EDITOR: &str = "dock.ed";

/// Lines of context around a change in a diff.
const CONTEXT: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// Tern (or Carly) submits text through the composer.
    Send(String),
    /// A click asked for the keys.
    Focus,
    /// An approval button of the tool waiting for permission.
    Answer(Approval),
}

/// What the session runs in, for the head row and the status bar.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// The agent's product name (`Claude Code`).
    pub product: String,
    pub sandbox: String,
    /// The primary workspace as the person knows it (`~/src/app`).
    pub workspace: Option<String>,
    pub extra_mounts: usize,
    pub mcp: Vec<String>,
}

pub fn view(t: &Transcript, composer: &Composer, ctx: &Context) -> View<Msg> {
    View::new().main(ui::col().children(transcript(t, ctx))).dock(ui::col().children(dock(t, composer, ctx)))
}

fn transcript(t: &Transcript, ctx: &Context) -> Vec<Node<Msg>> {
    let mut nodes = vec![head(t, ctx)];
    let mut previous: Option<&Entry> = None;
    for (i, entry) in t.entries.iter().enumerate() {
        let key = format!("e{i}");
        let node: Node<Msg> = match entry {
            Entry::User(text) => ui::message().key(key).side("end").child(ui::md(text.clone()).key("t")).into(),
            Entry::Agent(text) => {
                // A reply after the agent's own tools and thoughts keeps to one header.
                let continued = matches!(previous, Some(Entry::Agent(_) | Entry::Tool(_) | Entry::Thought(_)));
                ui::message()
                    .key(key)
                    .side("start")
                    .author(ctx.product.clone())
                    .continued(continued)
                    .child(ui::md(text.clone()).key("t"))
                    .into()
            }
            Entry::Thought(thought) => {
                let trace = ui::trace().key(key).variant("card").open(thought.took.is_none());
                let trace = match thought.took {
                    Some(took) => trace.status("done").took(millis(took)),
                    None => trace.status("running").age(0u64),
                };
                trace.child(ui::md(thought.text.clone()).key("t")).into()
            }
            Entry::Tool(tool) => tool_node(key, tool, t.permission.as_ref().filter(|p| p.tool == tool.id)),
            Entry::Error { text, detail } => {
                let card = ui::card().key(key).tone("error").icon("warn").head(text.clone());
                match detail {
                    Some(detail) => card.child(ui::code(detail.clone()).lang("text").key("detail")).into(),
                    None => card.into(),
                }
            }
            Entry::Notice(text) => ui::text(vec![Span::new(text.clone()).style("muted")]).key(key).into(),
        };
        nodes.push(node);
        previous = Some(entry);
    }
    let working = match t.turn {
        Turn::Starting => Some(format!("Starting {} in sandbox {}", ctx.product, ctx.sandbox)),
        Turn::Working if t.permission.is_none() && !t.streaming() => Some("Working".to_string()),
        _ => None,
    };
    if let Some(text) = working {
        nodes.push(ui::shimmer(text).key("working").into());
    }
    nodes
}

/// The page's head row: what runs where, and how it is doing.
fn head(t: &Transcript, ctx: &Context) -> Node<Msg> {
    let mut meta = vec![format!("sandbox {}", ctx.sandbox)];
    meta.extend(ctx.workspace.clone());
    if ctx.extra_mounts > 0 {
        meta.push(format!("+{} mount{}", ctx.extra_mounts, if ctx.extra_mounts == 1 { "" } else { "s" }));
    }
    if !ctx.mcp.is_empty() {
        meta.push(format!("MCP {}", ctx.mcp.join(", ")));
    }
    let (state, tone) = match t.turn {
        Turn::Starting => ("Starting", "pending"),
        Turn::Idle => ("Ready", "success"),
        Turn::Working if t.permission.is_some() => ("Needs approval", "warning"),
        Turn::Working => ("Working", "info"),
        Turn::Exited => ("Exited", "error"),
    };
    let title = vec![
        Span::new(ctx.product.clone()).style("title"),
        Span::new(format!("  {}", meta.join(" · "))).style("muted"),
    ];
    ui::row()
        .key("head")
        .justify("between")
        .align("center")
        .child(ui::text(title).key("title"))
        .child(ui::badge(state).tone(tone).key("state"))
        .into()
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
        "execute" => "Run",
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

fn tool_node(key: String, tool: &ToolCall, permission: Option<&Permission>) -> Node<Msg> {
    let mut node = ui::tool(tool_name(&tool.kind)).key(key);
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
    let status = match tool.status {
        _ if permission.is_some() => "ask",
        ToolStatus::Pending => "pending",
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Failed => "error",
        ToolStatus::Cancelled => "cancelled",
    };
    node = node.status(status);
    node = match tool.finished {
        // Tern counts on from the first `age` it gets; an unchanged one sends nothing.
        None => node.age(0i64),
        Some(end) => node.took(millis(end.saturating_duration_since(tool.started))),
    };
    if !tool.content.is_empty() {
        node = node.collapsible(true).collapsed(tool.status == ToolStatus::Done).preview(ToolPreview::Tail(6));
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
            ToolContent::Text(text) => ui::md(text.clone()).key(i).into(),
            ToolContent::Diff { path, old, new } => {
                ui::diff().key(i).path(path.clone()).prop("hunks", hunks(old.as_deref(), new)).into()
            }
        };
        node = node.child(child);
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

fn dock(t: &Transcript, composer: &Composer, ctx: &Context) -> Vec<Node<Msg>> {
    let mut nodes = Vec::new();
    if !t.plan.is_empty() {
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
        let phase = Phase { id: "plan".into(), items, ..Phase::default() };
        nodes.push(ui::checklist().key("plan").mode("hud").phases(vec![phase]).into());
    }

    let placeholder = match t.turn {
        Turn::Starting => format!("Starting {}…", ctx.product),
        Turn::Working if t.permission.is_some() => "Enter allows · Esc denies".to_string(),
        Turn::Working => format!("Esc stops {}", ctx.product),
        Turn::Idle => format!("Ask {} · Enter sends · ⇧Enter new line · Ctrl+D quits", ctx.product),
        Turn::Exited => "Enter closes".to_string(),
    };
    nodes.push(
        ui::editor()
            .key("ed")
            .text(composer.draft.clone())
            .cursor(composer.cursor_utf16())
            .placeholder(placeholder)
            .sendable(t.turn == Turn::Idle)
            .typeahead(t.turn == Turn::Starting)
            .readonly(t.turn == Turn::Exited)
            .on_send(|text| Msg::Send(text.to_owned()))
            .on_focus(Msg::Focus)
            .into(),
    );

    let mut bar = ui::status()
        .key("bar")
        .child(ui::seg(ctx.product.clone()).icon("sparkle").key("agent"))
        .child(ui::seg(ctx.sandbox.clone()).icon("box").key("sandbox"));
    if let Some(mode) = t.mode_name() {
        let hint = if t.modes.len() > 1 { " · ⇧Tab" } else { "" };
        bar = bar.child(ui::seg(format!("{mode}{hint}")).side("right").key("mode"));
    }
    let (state, tone) = match t.turn {
        Turn::Starting => ("starting", "pending"),
        Turn::Idle => ("ready", "success"),
        Turn::Working => ("working", "info"),
        Turn::Exited => ("exited", "error"),
    };
    nodes.push(bar.child(ui::seg(state).side("right").tone(tone).key("state")).into());
    nodes
}

fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn ctx() -> Context {
        Context {
            product: "Claude Code".into(),
            sandbox: "claude-app".into(),
            workspace: Some("~/src/app".into()),
            extra_mounts: 2,
            mcp: vec!["github".into()],
        }
    }

    fn region(view: &View<Msg>, index: usize) -> &[Node<Msg>] {
        view.regions()[index].expect("region").child_nodes()
    }

    #[test]
    fn a_session_draws_messages_tools_and_the_dock() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.turn = Turn::Idle;
        t.prompt("fix it", now);
        t.apply(
            &json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "`cargo test`", "kind": "execute",
                "status": "in_progress", "rawInput": {"command": "cargo test"}}),
            now,
        );
        t.apply(&json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}), now);
        t.apply(&json!({"sessionUpdate": "plan", "entries": [{"content": "Fix", "status": "in_progress"}]}), now);
        let view = view(&t, &Composer::default(), &ctx());
        let main = region(&view, 0);
        let kinds: Vec<&str> = main.iter().map(Node::kind).collect();
        assert_eq!(kinds, ["row", "message", "tool", "message"]);
        assert_eq!(main[1].props()["side"], "end");
        let tool = main[2].props();
        assert_eq!((tool["name"].as_str(), tool["title"].as_str()), (Some("bash"), Some("Run")));
        assert_eq!((tool["target"].as_str(), tool["targetKind"].as_str()), (Some("cargo test"), Some("command")));
        assert_eq!(tool["status"], "running");
        assert_eq!(main[3].props()["author"], "Claude Code");
        assert_eq!(main[3].props()["continued"], true);

        let dock = region(&view, 1);
        let kinds: Vec<&str> = dock.iter().map(Node::kind).collect();
        assert_eq!(kinds, ["checklist", "editor", "status"]);
        assert_eq!(dock[0].props()["phases"][0]["items"][0]["status"], "active");
        assert_eq!(dock[1].props()["sendable"], false);
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
        let view = view(&t, &Composer::default(), &ctx());
        let main = region(&view, 0);
        let tool = main[1].props();
        assert_eq!(tool["status"], "ask");
        assert_eq!(tool["targetKind"], "path");
        assert_eq!(tool["ask"], json!({"allow": "Allow", "deny": "Reject", "always": false}));
        assert!(main.iter().all(|n| n.kind() != "shimmer"));
        assert_eq!(region(&view, 1)[0].props()["placeholder"], "Enter allows · Esc denies");
    }

    #[test]
    fn settled_thoughts_and_tools_fold_and_show_their_time() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.turn = Turn::Working;
        t.apply(&json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}}), now);
        let view_running = view(&t, &Composer::default(), &ctx());
        assert_eq!(region(&view_running, 0)[1].props()["status"], "running");
        let later = now + Duration::from_millis(2500);
        t.apply(
            &json!({"sessionUpdate": "tool_call", "toolCallId": "t", "kind": "read", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "file"}}]}),
            later,
        );
        let view = view(&t, &Composer::default(), &ctx());
        let main = region(&view, 0);
        assert_eq!((main[1].props()["took"].as_u64(), main[1].props()["open"].as_bool()), (Some(2500), Some(false)));
        assert_eq!(main[2].props()["took"], 0);
        assert_eq!(main[2].props()["collapsed"], true);
        assert_eq!(main.last().map(Node::kind), Some("shimmer"));
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
}
