//! Slash commands: the agent's own (ACP's `available_commands_update`, run by
//! sending them as a prompt) and sand's, which ACP offers as methods rather
//! than as commands: listing, resuming and forking sessions, starting a new one.

/// A command the composer completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    /// What it takes after its name (`[session id]`).
    pub hint: Option<String>,
    /// sand runs it, rather than sending it to the agent.
    pub builtin: bool,
}

/// sand's own commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// Browse the sessions, to resume one.
    Sessions,
    /// Resume the session given, or pick one.
    Resume(Option<String>),
    /// Leave this session for a new one.
    New,
    /// Go on in a copy of this session, leaving it as it is.
    Fork,
    /// Mount a host directory into the sandbox (`DIR`, `DIR:ro`, `DIR:rw`).
    Mount(Option<String>),
    Help,
}

const BUILTINS: &[(&str, &str, Option<&str>)] = &[
    ("sessions", "Browse this project's sessions and resume one", None),
    ("resume", "Resume a session (the picker without an id)", Some("[session id]")),
    ("new", "Start a new session", None),
    ("fork", "Go on in a copy of this session, leaving the original as it is", None),
    ("mount", "Mount a directory of this computer into the sandbox", Some("<dir>[:ro|:rw]")),
    ("help", "List the commands", None),
];

pub fn builtins() -> impl Iterator<Item = SlashCommand> {
    BUILTINS.iter().map(|(name, description, hint)| SlashCommand {
        name: name.to_string(),
        description: description.to_string(),
        hint: hint.map(str::to_string),
        builtin: true,
    })
}

/// sand's command `text` runs, if it names one (`/resume abc`).
pub fn parse(text: &str) -> Option<Builtin> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    Some(match name {
        "sessions" => Builtin::Sessions,
        "resume" => Builtin::Resume((!args.is_empty()).then(|| args.to_string())),
        "new" | "clear" => Builtin::New,
        "fork" => Builtin::Fork,
        "mount" => Builtin::Mount((!args.is_empty()).then(|| args.to_string())),
        "help" => Builtin::Help,
        _ => return None,
    })
}

/// The commands completing `draft` while it is a `/name` being typed:
/// sand's first, then the agent's, each starting with what was typed.
pub fn complete(draft: &str, agent: &[SlashCommand]) -> Vec<SlashCommand> {
    let Some(typed) = draft.strip_prefix('/') else { return Vec::new() };
    if typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    let builtin: Vec<SlashCommand> = builtins().collect();
    let shadowed = |c: &SlashCommand| !c.builtin && builtin.iter().any(|b| b.name == c.name);
    builtin
        .iter()
        .chain(agent.iter().filter(|c| !shadowed(c)))
        .filter(|c| c.name.starts_with(typed) && c.name != typed)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> SlashCommand {
        SlashCommand { name: name.into(), description: String::new(), hint: None, builtin: false }
    }

    #[test]
    fn sands_commands_are_recognized() {
        assert_eq!(parse("/sessions"), Some(Builtin::Sessions));
        assert_eq!(parse(" /resume  abc-123 "), Some(Builtin::Resume(Some("abc-123".into()))));
        assert_eq!(parse("/resume"), Some(Builtin::Resume(None)));
        assert_eq!(parse("/new"), Some(Builtin::New));
        assert_eq!(parse("/fork"), Some(Builtin::Fork));
        assert_eq!(parse("/mount ~/notes:ro"), Some(Builtin::Mount(Some("~/notes:ro".into()))));
        assert_eq!(parse("/mount"), Some(Builtin::Mount(None)));
        assert_eq!(parse("/init"), None);
        assert_eq!(parse("resume"), None);
    }

    #[test]
    fn completion_offers_sands_then_the_agents_commands() {
        let agent = [agent("review"), agent("resume"), agent("release-notes")];
        let names = |draft: &str| complete(draft, &agent).into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names("/re"), ["resume", "review", "release-notes"]);
        assert_eq!(names("/"), ["sessions", "resume", "new", "fork", "mount", "help", "review", "release-notes"]);
        assert!(names("/resume").is_empty(), "a command typed whole needs no completion");
        assert!(names("/review x").is_empty());
        assert!(names("hi").is_empty());
    }
}
