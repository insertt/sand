//! Embeds the commit sand was built from in `sand --version`.

use std::process::Command;

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let version = match git(&["rev-parse", "--short=10", "HEAD"]) {
        Some(commit) => format!("{version} ({commit})"),
        // Built outside a checkout, e.g. from a crate tarball.
        None => version.to_owned(),
    };
    println!("cargo:rustc-env=SAND_VERSION={version}");

    // Rebuild when HEAD moves: on checkout (HEAD) and on commit (the branch
    // ref, or packed-refs once git packs it).
    let mut watched = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    watched.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for path in watched {
        if let Some(path) = git(&["rev-parse", "--git-path", &path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let out = String::from_utf8(out.stdout).ok().filter(|_| out.status.success())?;
    Some(out.trim().to_owned()).filter(|s| !s.is_empty())
}
