//! Command-line interface.

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::editor::AcpClient;

const EXAMPLES: &str = "\
Examples:
  sand codex                                 Codex in the current directory
  sand codex . ../lib:ro ../docs:rw          extra mounts, read-only or read-write
  sand claude --mcp atlassian,gitlab         with MCP servers from `sbx mcp ls`
  sand claude --resume                       attach to the existing sandbox as it is
  sand claude --resume . ../lib:ro           attach, adding ../lib to the sandbox
  sand mount ../lib:ro                       add ../lib to this directory's sandbox, even mid-session
  sand umount ../lib                         remove it again
  sand claude --recreate                     rebuild the sandbox, asking what to keep
  sand claude --force-recreate               rebuild it from the command line alone
  sand codex --no-attach                     set the sandbox up without opening the agent
  sand codex --scratch                       shared scratch space, no project
  sand --acp                                 ACP agent on stdio (for editors)
  sand codex --install-acp zed               register `sand codex --acp` in Zed
  sand claude --install-acp tern             register `sand claude --acp` in Tern
  sand --open-config                         edit sand's config file
  sand claude -- --continue                  pass arguments to the agent";

#[derive(Debug, Parser)]
#[command(
    name = "sand",
    version = env!("SAND_VERSION"),
    about = "Run AI coding agents in Docker Sandboxes with one command",
    after_help = EXAMPLES,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub run: RunArgs,

    /// Echo every sbx command sand runs and show its output
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build, snapshot and list custom templates
    Template {
        #[command(subcommand)]
        action: TemplateCommand,
    },
    /// Show a sandbox: what sand recorded about it, whether it is running, and agent API and MCP server status
    #[command(alias = "describe")]
    Status(StatusArgs),
    /// Allow a sandbox network access to hosts its policy blocked
    Allow(AllowArgs),
    /// Mount directories into an existing sandbox, also while a session runs
    Mount(MountArgs),
    /// Unmount directories from an existing sandbox, also while a session runs
    Umount(MountArgs),
}

#[derive(Debug, Subcommand)]
pub enum TemplateCommand {
    /// Build a Dockerfile template from config.toml and load it into the sandbox runtime
    Build { name: String },
    /// Snapshot an existing sandbox as a reusable template
    Save { sandbox: String, name: String },
    /// List templates
    #[command(alias = "list")]
    Ls,
    /// Remove a built or saved template
    Rm { name: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ConflictChoice {
    /// Make the sandbox match the command line exactly (recreates it if needed)
    Replace,
    /// Keep the sandbox's current mounts and MCP servers and add the new ones
    Merge,
    /// Stop without changing anything
    Abort,
}

#[derive(Debug, Default, Args)]
pub struct RunArgs {
    /// Harness (claude, codex, opencode, …) followed by directories to mount.
    /// Append :ro or :rw to a directory (default :rw). The first directory is
    /// the primary workspace; with none given the current directory is used.
    #[arg(value_name = "HARNESS] [MOUNT")]
    pub targets: Vec<String>,

    /// Arguments passed to the agent
    #[arg(last = true, value_name = "AGENT_ARGS")]
    pub agent_args: Vec<String>,

    /// MCP servers to expose, comma-separated (names from `sbx mcp ls`)
    #[arg(long, value_name = "SERVERS", value_delimiter = ',')]
    pub mcp: Vec<String>,

    /// Attach to the existing sandbox, adding any mounts and MCP servers given; fail if there is none
    #[arg(long, conflicts_with_all = ["recreate", "force_recreate", "dedicated_scratch"])]
    pub resume: bool,

    /// Recreate the sandbox, asking whether to keep its history, mounts and MCP servers
    #[arg(long, conflicts_with = "force_recreate")]
    pub recreate: bool,

    /// Recreate the sandbox without asking: history, mounts and MCP servers not on the command line are dropped
    #[arg(long)]
    pub force_recreate: bool,

    /// Use the shared, persistent scratch workspace instead of a project
    #[arg(long, conflicts_with = "dedicated_scratch")]
    pub scratch: bool,

    /// Use a throwaway sandbox with its own scratch space, removed on exit
    #[arg(long)]
    pub dedicated_scratch: bool,

    /// Speak ACP on stdio instead of opening the agent's terminal UI
    #[arg(long)]
    pub acp: bool,

    /// Create or update the sandbox and start it, but do not open the agent
    #[arg(long, conflicts_with_all = ["acp", "dedicated_scratch", "agent_args"])]
    pub no_attach: bool,

    /// Sandbox name (default: <harness>-<directory>)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Register sand as an ACP agent in an editor or terminal, then exit
    #[arg(long, value_enum, value_name = "CLIENT")]
    pub install_acp: Option<AcpClient>,

    /// Open the config file in $VISUAL or $EDITOR (created if missing), then exit
    #[arg(long)]
    pub open_config: bool,

    /// Template to create the sandbox from (name from config.toml or `sand template ls`, or an image)
    #[arg(long, value_name = "TEMPLATE")]
    pub template: Option<String>,

    /// Answer for "the sandbox exists with a different setup" instead of asking
    #[arg(long, value_enum, value_name = "CHOICE")]
    pub on_conflict: Option<ConflictChoice>,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Harness and project directory identifying the sandbox (default: current directory)
    #[arg(value_name = "HARNESS] [DIR")]
    pub targets: Vec<String>,

    /// Sandbox name
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Look at the shared scratch sandbox
    #[arg(long)]
    pub scratch: bool,

    /// Print the status as JSON on stdout
    #[arg(long)]
    pub json: bool,

    /// Skip the live checks (agent API from inside the sandbox, MCP gateway); report only what is recorded
    #[arg(long)]
    pub no_probe: bool,
}

#[derive(Debug, Args)]
pub struct MountArgs {
    /// Harness followed by the directories to mount or unmount (:ro or :rw, default :rw).
    /// The sandbox is the current directory's, unless --name or --scratch picks another.
    #[arg(value_name = "HARNESS] [DIR", required = true)]
    pub targets: Vec<String>,

    /// Sandbox name
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Use the shared scratch sandbox
    #[arg(long)]
    pub scratch: bool,
}

#[derive(Debug, Args)]
pub struct AllowArgs {
    /// Harness and project directory identifying the sandbox (default: current directory)
    #[arg(value_name = "HARNESS] [DIR")]
    pub targets: Vec<String>,

    /// Hosts to allow without asking, comma-separated (example.com, example.com:443, *.example.com)
    #[arg(long, value_name = "HOSTS", value_delimiter = ',')]
    pub host: Vec<String>,

    /// Only print the blocked hosts
    #[arg(long, conflicts_with_all = ["host", "global"])]
    pub list: bool,

    /// Allow the hosts for every sandbox instead of only this one
    #[arg(long)]
    pub global: bool,

    /// Sandbox name
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Look at the shared scratch sandbox
    #[arg(long)]
    pub scratch: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn run(args: &[&str]) -> RunArgs {
        let cli = Cli::try_parse_from(std::iter::once("sand").chain(args.iter().copied())).unwrap();
        assert!(cli.command.is_none());
        cli.run
    }

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn verbose_is_accepted_everywhere() {
        let parse = |args: &[&str]| Cli::try_parse_from(args.iter().copied()).unwrap().verbose;
        assert!(parse(&["sand", "-v", "codex"]) && parse(&["sand", "codex", "--verbose"]));
        assert!(parse(&["sand", "status", "-v"]) && parse(&["sand", "template", "ls", "--verbose"]));
        assert!(!parse(&["sand", "codex"]) && !parse(&["sand", "codex", "--", "-v"]));
    }

    #[test]
    fn harness_mounts_and_flags() {
        let a = run(&["codex", ".", "../other:ro", "--mcp", "atlassian,gitlab", "--name", "x"]);
        assert_eq!(a.targets, ["codex", ".", "../other:ro"]);
        assert_eq!(a.mcp, ["atlassian", "gitlab"]);
        assert_eq!(a.name.as_deref(), Some("x"));
        assert!(run(&["--acp"]).acp && run(&["--acp"]).targets.is_empty());
        assert_eq!(run(&["codex", "--install-acp", "zed"]).install_acp, Some(AcpClient::Zed));
        assert_eq!(run(&["--install-acp", "jetbrains"]).install_acp, Some(AcpClient::Jetbrains));
        assert_eq!(run(&["claude", "--install-acp", "tern"]).install_acp, Some(AcpClient::Tern));
        assert!(run(&["--open-config"]).open_config && !run(&["codex"]).open_config);
    }

    #[test]
    fn agent_args_follow_a_double_dash() {
        let a = run(&["claude", ".", "--", "--continue", "fix it"]);
        assert_eq!(a.targets, ["claude", "."]);
        assert_eq!(a.agent_args, ["--continue", "fix it"]);
    }

    #[test]
    fn incompatible_flags_are_rejected() {
        let bad: &[&[&str]] = &[
            &["codex", "--resume", "--recreate"],
            &["codex", "--scratch", "--dedicated-scratch"],
            &["codex", "--no-preserve-history"],
            &["codex", "--recreate", "--force-recreate"],
            &["codex", "--resume", "--force-recreate"],
            &["codex", "--install-acp", "vim"],
            &["codex", "--install-editor", "zed"],
            &["codex", "--resume", "--dedicated-scratch"],
            &["codex", "--no-attach", "--acp"],
            &["codex", "--no-attach", "--dedicated-scratch"],
            &["codex", "--no-attach", "--", "--continue"],
        ];
        for args in bad {
            assert!(Cli::try_parse_from(std::iter::once("sand").chain(args.iter().copied())).is_err(), "{args:?}");
        }
        assert!(run(&["codex", "--force-recreate"]).force_recreate);
    }

    #[test]
    fn subcommands_parse() {
        let cli = Cli::try_parse_from(["sand", "template", "build", "rust"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Template { action: TemplateCommand::Build { ref name } }) if name == "rust")
        );
        let cli = Cli::try_parse_from(["sand", "status", "codex", "--name", "x"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Status(ref s)) if s.targets == ["codex"] && s.name.as_deref() == Some("x"))
        );
        let cli = Cli::try_parse_from(["sand", "describe", "claude", "--json", "--no-probe"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Status(ref s)) if s.targets == ["claude"] && s.json && s.no_probe));
        let cli = Cli::try_parse_from(["sand", "allow", "codex", "--host", "a.com,b.com:443", "--global"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Allow(ref a)) if a.targets == ["codex"] && a.host == ["a.com", "b.com:443"] && a.global)
        );
        assert!(Cli::try_parse_from(["sand", "allow", "--list", "--host", "a.com"]).is_err());
        let cli = Cli::try_parse_from(["sand", "mount", "claude", "../lib:ro", "--name", "x"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Mount(ref m)) if m.targets == ["claude", "../lib:ro"] && m.name.as_deref() == Some("x"))
        );
        assert!(matches!(Cli::try_parse_from(["sand", "umount", "../lib"]).unwrap().command, Some(Command::Umount(_))));
        assert!(Cli::try_parse_from(["sand", "mount"]).is_err());
    }
}
