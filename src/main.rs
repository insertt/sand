//! sand — a wrapper around Docker Sandboxes (`sbx`) for day-to-day agent work.

mod acp;
mod cli;
mod config;
mod editor;
mod harness;
mod lock;
mod log;
mod mcp;
mod mounts;
mod paths;
mod policy;
mod runtime;
mod sbx;
mod session;
mod state;
mod sync;
mod template;

use std::process::ExitCode;
use std::sync::atomic::Ordering;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, TemplateCommand};
use config::Config;
use paths::Dirs;
use sbx::Sbx;

fn dispatch(cli: Cli) -> Result<i32> {
    let dirs = Dirs::discover()?;
    let config = Config::load(&dirs)?;
    match cli.command {
        None => session::run(cli.run, &dirs, &config),
        Some(Command::Status(args)) => session::status(args, &dirs, &config),
        Some(Command::Allow(args)) => policy::allow(args, &dirs, &config),
        Some(Command::Template { action }) => {
            let sbx = Sbx::new(true);
            match action {
                TemplateCommand::Build { name } => template::build(&sbx, &dirs, &config, &name)?,
                TemplateCommand::Save { sandbox, name } => template::save(&sbx, &dirs, &config, &sandbox, &name)?,
                TemplateCommand::Ls => template::list(&dirs, &config)?,
                TemplateCommand::Rm { name } => template::remove(&sbx, &dirs, &name)?,
            }
            Ok(0)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // While the agent owns the terminal, Ctrl-C is the agent's to handle and
    // sand has to outlive it to release the sandbox cleanly.
    let _ = ctrlc::set_handler(|| {
        if !session::IN_SESSION.load(Ordering::SeqCst) {
            eprintln!();
            log::error("interrupted");
            std::process::exit(130);
        }
    });
    match dispatch(cli) {
        Ok(code) => ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1)),
        Err(err) => {
            log::error(format!("{err:#}"));
            ExitCode::FAILURE
        }
    }
}
