//! Step-by-step progress output. Everything goes to stderr so that stdout stays
//! free for the agent (and is a clean ACP stream in `--acp` mode).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use console::{Term, style};

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// One line of progress output, for a sink that shows it somewhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Step(String),
    Info(String),
    Ok(String),
    Warn(String),
    Error(String),
    /// An external command (`--verbose` only).
    Command(String),
    /// Something slow is under way (a spinner on the terminal).
    Wait(String),
    /// What a failed command printed.
    Output(String),
}

type Sink = Box<dyn Fn(Line) + Send + Sync>;

/// Where progress goes instead of stderr, while a native session draws it.
static SINK: RwLock<Option<Sink>> = RwLock::new(None);

pub fn set_sink(sink: Option<Sink>) {
    *SINK.write().unwrap_or_else(PoisonError::into_inner) = sink;
}

/// Hands `line` to the sink; false when there is none.
fn emit(line: impl FnOnce() -> Line) -> bool {
    match SINK.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
        Some(sink) => {
            sink(line());
            true
        }
        None => false,
    }
}

/// Prints `line` the way it would have been printed without a sink.
pub fn replay(line: &Line) {
    match line {
        Line::Step(m) => step(m),
        Line::Info(m) | Line::Wait(m) => info(m),
        Line::Ok(m) => ok(m),
        Line::Warn(m) => warn(m),
        Line::Error(m) => error(m),
        Line::Command(m) => command(m),
        Line::Output(m) => eprint!("{m}"),
    }
}

/// What a failed command printed: shown where the progress goes.
pub fn output(text: &str) {
    if !emit(|| Line::Output(text.to_string())) {
        eprint!("{text}");
    }
}

/// Turns on the output of `--verbose`: echoed commands and what they print.
pub fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn step(message: impl AsRef<str>) {
    if emit(|| Line::Step(message.as_ref().to_string())) {
        return;
    }
    eprintln!("{} {}", style("==>").cyan().bold().for_stderr(), style(message.as_ref()).bold().for_stderr());
}

pub fn info(message: impl AsRef<str>) {
    if emit(|| Line::Info(message.as_ref().to_string())) {
        return;
    }
    eprintln!("    {}", message.as_ref());
}

/// Echoes an external command before it runs (only with `--verbose`).
pub fn command(line: impl AsRef<str>) {
    if !verbose() || emit(|| Line::Command(line.as_ref().to_string())) {
        return;
    }
    eprintln!("    {} {}", style("$").dim().for_stderr(), style(line.as_ref()).dim().for_stderr());
}

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A line that keeps moving while a slow command runs; dropping it erases the line.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Shows `message` with a spinner and the time elapsed so far. Does nothing
/// when stderr is not a terminal, where the line could not be redrawn.
pub fn spinner(message: impl AsRef<str>) -> Spinner {
    let stop = Arc::new(AtomicBool::new(false));
    if emit(|| Line::Wait(message.as_ref().to_string())) {
        return Spinner { stop, thread: None };
    }
    let thread = console::user_attended_stderr().then(|| {
        let (stop, message) = (Arc::clone(&stop), message.as_ref().to_string());
        thread::spawn(move || {
            let (term, started) = (Term::stderr(), Instant::now());
            for frame in SPINNER_FRAMES.iter().cycle() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let elapsed = style(elapsed(started.elapsed())).dim().for_stderr();
                let _ = term.write_str(&format!("\r    {} {message} {elapsed}", style(frame).cyan().for_stderr()));
                thread::sleep(Duration::from_millis(80));
            }
            let _ = term.clear_line();
        })
    });
    Spinner { stop, thread }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn elapsed(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) }
}

pub fn ok(message: impl AsRef<str>) {
    if emit(|| Line::Ok(message.as_ref().to_string())) {
        return;
    }
    eprintln!("    {} {}", style("✓").green().for_stderr(), message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    if emit(|| Line::Warn(message.as_ref().to_string())) {
        return;
    }
    eprintln!("    {} {}", style("!").yellow().bold().for_stderr(), message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    if emit(|| Line::Error(message.as_ref().to_string())) {
        return;
    }
    eprintln!("{} {}", style("error:").red().bold().for_stderr(), message.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_switches_to_minutes() {
        assert_eq!(elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed(Duration::from_secs(60)), "1m 00s");
        assert_eq!(elapsed(Duration::from_secs(605)), "10m 05s");
    }
}
