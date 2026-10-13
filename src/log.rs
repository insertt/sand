//! Step-by-step progress output. Everything goes to stderr so that stdout stays
//! free for the agent (and is a clean ACP stream in `--acp` mode).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use console::{Term, style};

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Turns on the output of `--verbose`: echoed commands and what they print.
pub fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn step(message: impl AsRef<str>) {
    eprintln!("{} {}", style("==>").cyan().bold().for_stderr(), style(message.as_ref()).bold().for_stderr());
}

pub fn info(message: impl AsRef<str>) {
    eprintln!("    {}", message.as_ref());
}

/// Echoes an external command before it runs (only with `--verbose`).
pub fn command(line: impl AsRef<str>) {
    if !verbose() {
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
    eprintln!("    {} {}", style("✓").green().for_stderr(), message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!("    {} {}", style("!").yellow().bold().for_stderr(), message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
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
