//! Step-by-step progress output. Everything goes to stderr so that stdout stays
//! free for the agent (and is a clean ACP stream in `--acp` mode).

use console::style;

pub fn step(message: impl AsRef<str>) {
    eprintln!("{} {}", style("==>").cyan().bold().for_stderr(), style(message.as_ref()).bold().for_stderr());
}

pub fn info(message: impl AsRef<str>) {
    eprintln!("    {}", message.as_ref());
}

/// Echoes an external command before it runs.
pub fn command(line: impl AsRef<str>) {
    eprintln!("    {} {}", style("$").dim().for_stderr(), style(line.as_ref()).dim().for_stderr());
}

pub fn ok(message: impl AsRef<str>) {
    eprintln!("    {} {}", style("✓").green().for_stderr(), message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!("    {} {}", style("!").yellow().bold().for_stderr(), message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    eprintln!("{} {}", style("error:").red().bold().for_stderr(), message.as_ref());
}
