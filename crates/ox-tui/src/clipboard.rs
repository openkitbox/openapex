//! Reading and writing the system clipboard. This is the only module that
//! spawns clipboard subprocesses (`osascript`/`pbpaste`/`pbcopy` on macOS,
//! `wl-paste`/`wl-copy`/`xclip` on Linux). Which commands to try
//! ([`read_plan`], [`write_plan`]), what their output means ([`classify`]) and
//! the terminal-side copy sequence ([`osc52`]) are pure; [`read`], [`write`]
//! and [`run_with_timeout`] are the I/O edge, run by the executor under
//! `spawn_blocking`. Writing never prints to the terminal: the OSC 52 string is
//! handed back to the caller, which owns the terminal.
//!
//! Deviations from upstream `clipboard.ts` (which uses OpenTUI's host
//! clipboard): other platforms answer [`Clip::Unsupported`] instead of reading
//! through the terminal, and macOS images come from `osascript`'s
//! `«data PNGf…»` hex text rather than a temp file, so nothing touches disk.

use std::borrow::Cow;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ox_schema::support::FileInput;

/// Largest image or text taken from the clipboard (same budget as pasted files).
pub const MAX_CLIPBOARD_BYTES: usize = 20 * 1024 * 1024;
/// How long one clipboard command may run before it is killed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// What a command may print: osascript writes the PNG as hex, twice the size.
const MAX_OUTPUT_BYTES: usize = 2 * MAX_CLIPBOARD_BYTES + 64;
/// Largest OSC 52 base64 payload sent; some terminals drop bigger ones.
const MAX_OSC52_PAYLOAD: usize = 75_000;
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// What the clipboard held.
#[derive(Debug, Clone, PartialEq)]
pub enum Clip {
    /// A PNG, ready to attach (`data:image/png;base64,…`, named `clipboard`).
    Image(FileInput),
    Text(String),
    /// Neither an image nor text, or every command failed.
    Empty,
    /// This platform has no clipboard command.
    Unsupported,
}

/// How a command prints what it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Raw PNG bytes.
    PngBytes,
    /// osascript's `«data PNGf<hex>»`.
    PngHex,
    Text,
}

/// One clipboard command to try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cmd {
    pub program: &'static str,
    pub args: &'static [&'static str],
    pub format: Format,
}

/// The commands to try on `os` (a `std::env::consts::OS` value), images before
/// text, first usable answer wins. Empty means unsupported.
pub fn read_plan(os: &str) -> Vec<Cmd> {
    const OSASCRIPT: Cmd = Cmd {
        program: "osascript",
        args: &["-e", "the clipboard as «class PNGf»"],
        format: Format::PngHex,
    };
    const PBPASTE: Cmd = Cmd {
        program: "pbpaste",
        args: &[],
        format: Format::Text,
    };
    const WL_IMAGE: Cmd = Cmd {
        program: "wl-paste",
        args: &["--no-newline", "--type", "image/png"],
        format: Format::PngBytes,
    };
    const XCLIP_IMAGE: Cmd = Cmd {
        program: "xclip",
        args: &["-selection", "clipboard", "-t", "image/png", "-o"],
        format: Format::PngBytes,
    };
    const WL_TEXT: Cmd = Cmd {
        program: "wl-paste",
        args: &["--no-newline"],
        format: Format::Text,
    };
    const XCLIP_TEXT: Cmd = Cmd {
        program: "xclip",
        args: &["-selection", "clipboard", "-o"],
        format: Format::Text,
    };
    match os {
        "macos" => vec![OSASCRIPT, PBPASTE],
        "linux" => vec![WL_IMAGE, XCLIP_IMAGE, WL_TEXT, XCLIP_TEXT],
        _ => Vec::new(),
    }
}

/// What `output` of a command printing `format` means. [`Clip::Empty`] when it
/// holds no PNG (bad magic, bad hex) or no text, or is over the size cap.
pub fn classify(format: Format, output: Vec<u8>) -> Clip {
    match format {
        Format::PngBytes => png_clip(&output),
        Format::PngHex => {
            let png = decode_osascript_png(&output);
            // The hex is twice the PNG; free it before the data URI is built.
            drop(output);
            png.map_or(Clip::Empty, |png| png_clip(&png))
        }
        Format::Text => match String::from_utf8(output) {
            Ok(text) if !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES => Clip::Text(text),
            _ => Clip::Empty,
        },
    }
}

fn png_clip(bytes: &[u8]) -> Clip {
    if bytes.len() > MAX_CLIPBOARD_BYTES || !bytes.starts_with(&PNG_MAGIC) {
        return Clip::Empty;
    }
    Clip::Image(FileInput {
        uri: crate::attachment::data_uri("image/png", bytes),
        source: None,
        name: Some("clipboard".to_owned()),
        description: None,
        mention: None,
    })
}

/// The bytes inside osascript's `«data PNGf<hex>»`, `None` for anything else.
fn decode_osascript_png(output: &[u8]) -> Option<Vec<u8>> {
    let hex = output
        .trim_ascii()
        .strip_prefix("«data PNGf".as_bytes())?
        .strip_suffix("»".as_bytes())?;
    if hex.len() % 2 != 0 || hex.len() / 2 > MAX_CLIPBOARD_BYTES {
        return None;
    }
    // `collect` into an `Option<Vec>` cannot size ahead; the PNG may be tens of MB.
    let mut png = Vec::with_capacity(hex.len() / 2);
    for &[hi, lo] in hex.as_chunks::<2>().0 {
        png.push(hex_digit(hi)? << 4 | hex_digit(lo)?);
    }
    Some(png)
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|d| d as u8)
}

/// The commands that copy stdin to the clipboard on `os`, first working one
/// wins. Empty means unsupported.
pub fn write_plan(os: &str) -> Vec<(&'static str, &'static [&'static str])> {
    match os {
        "macos" => vec![("pbcopy", &[])],
        "linux" => vec![
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard", "-i"]),
        ],
        _ => Vec::new(),
    }
}

/// `text` without NUL bytes (neither the host clipboard nor OSC 52 can carry
/// them); borrowed when there are none.
fn without_nul(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', ""))
    } else {
        Cow::Borrowed(text)
    }
}

/// The terminal sequence (`ESC ] 52 ; c ; <base64> BEL`) that makes the
/// terminal set its clipboard, which reaches the local machine over ssh.
/// `None` for empty text or a payload over [`MAX_OSC52_PAYLOAD`].
pub fn osc52(text: &str) -> Option<String> {
    let text = without_nul(text);
    if text.is_empty() || text.len().div_ceil(3) * 4 > MAX_OSC52_PAYLOAD {
        return None;
    }
    Some(format!(
        "\x1b]52;c;{}\x07",
        STANDARD.encode(text.as_bytes())
    ))
}

/// Copy `text` to the host clipboard with the first command of
/// [`write_plan`] that accepts it. Blocks; nothing is printed.
pub fn write(text: &str) -> std::io::Result<()> {
    let text = without_nul(text);
    for (program, args) in write_plan(std::env::consts::OS) {
        if pipe_into(program, args, text.as_bytes(), COMMAND_TIMEOUT) {
            return Ok(());
        }
    }
    Err(std::io::Error::other(
        "no clipboard command accepted the text",
    ))
}

/// Run `program`, feed it `input` on stdin, and report whether it exited
/// successfully within `timeout` (it is killed after that). The input is
/// written from a helper thread so a child that stops reading cannot stall us.
fn pipe_into(program: &str, args: &[&str], input: &[u8], timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let deadline = Instant::now() + timeout;
    std::thread::scope(|scope| {
        scope.spawn(move || {
            // A broken pipe means the child exited early; its status says why.
            let _ = stdin.write_all(input);
        });
        let ok = wait_until(&mut child, deadline) == Some(true);
        if !ok {
            let _ = child.kill();
            let _ = child.wait();
        }
        ok
    })
}

/// Read the clipboard with the commands of this platform. Blocks.
pub fn read() -> Clip {
    let plan = read_plan(std::env::consts::OS);
    if plan.is_empty() {
        return Clip::Unsupported;
    }
    for cmd in &plan {
        let Some(output) = run_with_timeout(cmd, COMMAND_TIMEOUT) else {
            continue;
        };
        match classify(cmd.format, output) {
            Clip::Empty => {}
            usable => return usable,
        }
    }
    Clip::Empty
}

/// Run `cmd`, returning what it printed on stdout, or `None` when it cannot
/// start, exits non-zero, prints more than [`MAX_OUTPUT_BYTES`], or is still
/// running after `timeout` (it is killed then).
///
/// Stdout is drained on a helper thread so a full pipe cannot stall the child.
/// After a kill the thread ends when the pipe closes; a grandchild that holds
/// the pipe open only delays that, never this call.
pub fn run_with_timeout(cmd: &Cmd, timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(cmd.program)
        .args(cmd.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes);
        let _ = tx.send(read.ok().map(|_| bytes));
    });
    let output = rx.recv_timeout(timeout).ok().flatten();
    let status = output
        .as_ref()
        .and_then(|_| wait_until(&mut child, deadline));
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    output.filter(|bytes| bytes.len() <= MAX_OUTPUT_BYTES && status == Some(true))
}

/// `Some(success)` once the child has exited, `None` if it outlives `deadline`.
fn wait_until(child: &mut Child, deadline: Instant) -> Option<bool> {
    loop {
        if let Some(status) = child.try_wait().ok()? {
            return Some(status.success());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
