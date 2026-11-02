//! Images from the system clipboard. Tern tells a program that something was
//! pasted, but not an image's bytes (an image-only paste arrives as an empty
//! paste), so sand reads the clipboard of the machine it runs on: with
//! osascript on macOS, wl-paste or xclip on Linux, PowerShell on Windows.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The largest image sand attaches (Tern's blobs stop at 16 MiB).
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// How long the clipboard tools may take, all together.
const DEADLINE: Duration = Duration::from_secs(6);

/// The MIME type of an image sand can attach, told by its first bytes.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// A PNG's width and height, from its header.
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if sniff(bytes) != Some("image/png") || bytes.len() < 24 {
        return None;
    }
    let read = |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    Some((read(16), read(20)))
}

/// The image on the clipboard, with its MIME type.
pub fn image() -> Result<(Vec<u8>, &'static str), String> {
    if ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"].iter().any(|k| std::env::var_os(k).is_some()) {
        return Err("over ssh, sand cannot read your computer's clipboard".to_string());
    }
    let started = Instant::now();
    let mut tried = false;
    for mut command in commands() {
        if started.elapsed() >= DEADLINE {
            break;
        }
        tried = true;
        let Ok(output) = command.stdin(Stdio::null()).stderr(Stdio::null()).output() else { continue };
        let bytes = output.stdout;
        if !output.status.success() || bytes.is_empty() {
            continue;
        }
        if bytes.len() > MAX_BYTES {
            return Err(format!("the image on the clipboard is over {} MiB", MAX_BYTES / 1024 / 1024));
        }
        if let Some(mime) = sniff(&bytes) {
            return Ok((bytes, mime));
        }
    }
    if !tried {
        return Err(missing_tool().to_string());
    }
    Err("there is no image on the clipboard".to_string())
}

#[cfg(target_os = "macos")]
fn commands() -> Vec<Command> {
    // As hermes-for-tern does: the clipboard as PNG, written to a file named
    // as the script's argument (never part of its text), then printed.
    let Ok(dir) = tempfile_dir() else { return Vec::new() };
    let path = dir.join("clipboard.png");
    let script = "on run argv\n\
        set destination to POSIX file (item 1 of argv)\n\
        set picture to the clipboard as \u{ab}class PNGf\u{bb}\n\
        set handle to open for access destination with write permission\n\
        try\n\
        set eof handle to 0\n\
        write picture to handle\n\
        close access handle\n\
        on error problem\n\
        close access handle\n\
        error problem\n\
        end try\n\
        end run";
    let mut command = Command::new("sh");
    command
        .args(["-c", "osascript -e \"$1\" \"$2\" >/dev/null && cat \"$2\"; rm -rf \"$3\"", "sh", script])
        .arg(&path)
        .arg(&dir)
        .stdout(Stdio::piped());
    vec![command]
}

/// A private directory for one clipboard read.
#[cfg(target_os = "macos")]
fn tempfile_dir() -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!("sand-clipboard-{}-{}", std::process::id(), rand_suffix()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

#[cfg(target_os = "macos")]
fn rand_suffix() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

#[cfg(windows)]
fn commands() -> Vec<Command> {
    let script = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
        $i = [System.Windows.Forms.Clipboard]::GetImage(); if ($i -eq $null) { exit 1 }; \
        $m = New-Object System.IO.MemoryStream; $i.Save($m, [System.Drawing.Imaging.ImageFormat]::Png); \
        $o = [Console]::OpenStandardOutput(); $b = $m.ToArray(); $o.Write($b, 0, $b.Length)";
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script]).stdout(Stdio::piped());
    vec![command]
}

#[cfg(not(any(target_os = "macos", windows)))]
fn commands() -> Vec<Command> {
    let mut found = Vec::new();
    let on_path = |tool: &str| {
        std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
    };
    for mime in ["image/png", "image/jpeg"] {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && on_path("wl-paste") {
            let mut command = Command::new("wl-paste");
            command.args(["--no-newline", "--type", mime]).stdout(Stdio::piped());
            found.push(command);
        }
        if std::env::var_os("DISPLAY").is_some() && on_path("xclip") {
            let mut command = Command::new("xclip");
            command.args(["-selection", "clipboard", "-t", mime, "-o"]).stdout(Stdio::piped());
            found.push(command);
        }
    }
    found
}

fn missing_tool() -> &'static str {
    if cfg!(any(target_os = "macos", windows)) {
        "the clipboard could not be read"
    } else {
        "reading images from the clipboard needs wl-paste (Wayland) or xclip (X11)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_told_by_their_bytes() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(sniff(&png), Some("image/png"));
        assert_eq!(png_size(&png), Some((640, 480)));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"hello"), None);
        assert_eq!(png_size(&[0xff, 0xd8, 0xff]), None);
    }
}
