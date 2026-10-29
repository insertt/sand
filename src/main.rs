//! sand — a wrapper around Docker Sandboxes (`sbx`) for day-to-day agent work.

mod acp;
mod cli;
mod config;
mod describe;
mod editor;
mod harness;
mod lock;
mod log;
mod mcp;
mod mounts;
mod paths;
mod policy;
mod registry;
mod runtime;
mod sbx;
mod session;
mod state;
mod sync;
mod template;
mod vertex;

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
    // Before the config is loaded, so that a broken one can still be fixed.
    if cli.command.is_none() && cli.run.open_config {
        config::open_in_editor(&dirs)?;
        return Ok(0);
    }
    let config = Config::load(&dirs)?;
    match cli.command {
        None => session::run(cli.run, &dirs, &config),
        Some(Command::Status(args)) => session::status(args, &dirs, &config),
        Some(Command::Describe(args)) => describe::describe(args, &dirs, &config),
        Some(Command::Allow(args)) => policy::allow(args, &dirs, &config),
        Some(Command::Mount(args)) => session::change_mounts(args, true, &dirs, &config),
        Some(Command::Umount(args)) => session::change_mounts(args, false, &dirs, &config),
        Some(Command::Template { action }) => {
            // `template ls` reads sand's own records and needs no sbx.
            let sbx = || Sbx::connect(true);
            match action {
                TemplateCommand::Build { name } => template::build(&sbx()?, &dirs, &config, &name)?,
                TemplateCommand::Save { sandbox, name } => template::save(&sbx()?, &dirs, &config, &sandbox, &name)?,
                TemplateCommand::Ls => template::list(&dirs, &config)?,
                TemplateCommand::Rm { name } => template::remove(&sbx()?, &dirs, &name)?,
            }
            Ok(0)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    log::set_verbose(cli.verbose);
    // While the agent owns the terminal, Ctrl-C is the agent's to handle and
    // sand has to outlive it to release the sandbox cleanly.
    let _ = ctrlc::set_handler(|| {
        if !session::IN_SESSION.load(Ordering::SeqCst) {
            eprintln!();
            log::error("interrupted");
            session::dispose();
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
