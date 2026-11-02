//! Images from the system clipboard. Tern tells a program that something was
//! pasted, but not an image's bytes (an image-only paste arrives as an empty
//! paste), so sand reads the clipboard of the machine it runs on: through
//! AppKit (osascript) on macOS, wl-paste or xclip on Linux, PowerShell on
//! Windows.

use std::path::{Path, PathBuf};
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

/// One way to read the clipboard: a command, and where it leaves the image
/// (its stdout, or a file).
struct Attempt {
    command: Command,
    file: Option<PathBuf>,
}

impl From<Command> for Attempt {
    fn from(command: Command) -> Self {
        Self { command, file: None }
    }
}

/// The image on the clipboard, with its MIME type. A failure says why, in
/// the clipboard tool's own words when it has some.
pub fn image() -> Result<(Vec<u8>, &'static str), String> {
    if ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"].iter().any(|k| std::env::var_os(k).is_some()) {
        return Err("over ssh, sand cannot read your computer's clipboard".to_string());
    }
    let started = Instant::now();
    let attempts = attempts();
    if attempts.is_empty() {
        return Err(missing_tool().to_string());
    }
    let mut why = None;
    for Attempt { mut command, file } in attempts {
        if started.elapsed() >= DEADLINE {
            break;
        }
        let output = match command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output() {
            Ok(output) => output,
            Err(err) => {
                why = Some(format!("{}: {err}", command.get_program().to_string_lossy()));
                continue;
            }
        };
        let bytes = match &file {
            Some(path) => std::fs::read(path).unwrap_or_default(),
            None => output.stdout,
        };
        if let Some(dir) = file.as_deref().and_then(Path::parent) {
            let _ = std::fs::remove_dir_all(dir);
        }
        if !output.status.success() || bytes.is_empty() {
            let said = String::from_utf8_lossy(&output.stderr);
            if let Some(line) = said.lines().map(str::trim).rfind(|l| !l.is_empty()) {
                why = Some(tidy(line));
            }
            continue;
        }
        if bytes.len() > MAX_BYTES {
            return Err(format!("the image on the clipboard is over {} MiB", MAX_BYTES / 1024 / 1024));
        }
        if let Some(mime) = sniff(&bytes) {
            return Ok((bytes, mime));
        }
        why = Some("the clipboard holds something that is not a PNG, JPEG, GIF or WebP image".to_string());
    }
    Err(why.unwrap_or_else(|| "there is no image on the clipboard".to_string()))
}

/// A tool's error line without its wrapping (osascript says
/// `execution error: Error: … (-2700)`).
fn tidy(line: &str) -> String {
    let line = line.strip_prefix("execution error: ").unwrap_or(line);
    let line = line.strip_prefix("Error: ").unwrap_or(line);
    let line = match line.rfind(" (-") {
        Some(at) if line.ends_with(')') => &line[..at],
        _ => line,
    };
    line.trim_end_matches('.').to_string()
}

/// The pasteboard's image, as PNG: its PNG if it has one, else whatever image
/// AppKit can read from it (TIFF, PDF, HEIC, JPEG, a copied image file),
/// converted. Written to the file named by the script's argument; an error
/// names the pasteboard's types, to tell what was there instead.
#[cfg(target_os = "macos")]
const MACOS_SCRIPT: &str = r#"
ObjC.import('AppKit');
function run(argv) {
    const board = $.NSPasteboard.generalPasteboard;
    let png = board.dataForType($.NSPasteboardTypePNG);
    if (png.isNil()) {
        let image = $();
        const images = board.readObjectsForClassesOptions($([$.NSImage]), $({}));
        if (!images.isNil() && images.count > 0) {
            image = images.objectAtIndex(0);
        } else {
            const urls = board.readObjectsForClassesOptions($([$.NSURL]), $({NSPasteboardURLReadingFileURLsOnlyKey: true}));
            if (!urls.isNil() && urls.count > 0) {
                image = $.NSImage.alloc.initWithContentsOfURL(urls.objectAtIndex(0));
            }
        }
        if (image.isNil()) {
            const types = ObjC.deepUnwrap(board.types) || [];
            throw new Error('there is no image on the clipboard (it holds: ' + (types.join(', ') || 'nothing') + ')');
        }
        const rep = $.NSBitmapImageRep.imageRepWithData(image.TIFFRepresentation);
        png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
    }
    if (!png.writeToFileAtomically(argv[0], true)) {
        throw new Error('the image could not be written to a file');
    }
}
"#;

#[cfg(target_os = "macos")]
fn attempts() -> Vec<Attempt> {
    let Ok(dir) = private_dir() else { return Vec::new() };
    let file = dir.join("clipboard.png");
    let mut command = Command::new("osascript");
    command.args(["-l", "JavaScript", "-e", MACOS_SCRIPT]).arg(&file);
    vec![Attempt { command, file: Some(file) }]
}

/// A private directory for one clipboard read.
#[cfg(target_os = "macos")]
fn private_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("sand-clipboard-{}-{nanos}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

#[cfg(windows)]
fn attempts() -> Vec<Attempt> {
    let script = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
        $i = [System.Windows.Forms.Clipboard]::GetImage(); if ($i -eq $null) { exit 1 }; \
        $m = New-Object System.IO.MemoryStream; $i.Save($m, [System.Drawing.Imaging.ImageFormat]::Png); \
        $o = [Console]::OpenStandardOutput(); $b = $m.ToArray(); $o.Write($b, 0, $b.Length)";
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script]);
    vec![command.into()]
}

#[cfg(not(any(target_os = "macos", windows)))]
fn attempts() -> Vec<Attempt> {
    let mut found = Vec::new();
    let on_path = |tool: &str| {
        std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
    };
    for mime in ["image/png", "image/jpeg"] {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && on_path("wl-paste") {
            let mut command = Command::new("wl-paste");
            command.args(["--no-newline", "--type", mime]);
            found.push(command.into());
        }
        if std::env::var_os("DISPLAY").is_some() && on_path("xclip") {
            let mut command = Command::new("xclip");
            command.args(["-selection", "clipboard", "-t", mime, "-o"]);
            found.push(command.into());
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

    #[test]
    fn tool_errors_read_plainly() {
        assert_eq!(
            tidy(
                "execution error: Error: there is no image on the clipboard (it holds: public.utf8-plain-text) (-2700)"
            ),
            "there is no image on the clipboard (it holds: public.utf8-plain-text)"
        );
        assert_eq!(tidy("Error: target image/png not available"), "target image/png not available");
    }
}
