//! Reading the system clipboard. This is the only module that spawns
//! clipboard-reading subprocesses (`osascript`/`pbpaste` on macOS,
//! `wl-paste`/`xclip` on Linux). Which commands to try ([`read_plan`]) and what
//! their output means ([`classify`]) are pure; [`read`] and
//! [`run_with_timeout`] are the I/O edge, run by the executor under
//! `spawn_blocking`.
//!
//! Deviations from upstream `clipboard.ts` (which uses OpenTUI's host
//! clipboard): other platforms answer [`Clip::Unsupported`] instead of reading
//! through the terminal, and macOS images come from `osascript`'s
//! `«data PNGf…»` hex text rather than a temp file, so nothing touches disk.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ox_schema::support::FileInput;

/// Largest image or text taken from the clipboard (same budget as pasted files).
pub const MAX_CLIPBOARD_BYTES: usize = 20 * 1024 * 1024;
/// How long one clipboard command may run before it is killed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// What a command may print: osascript writes the PNG as hex, twice the size.
const MAX_OUTPUT_BYTES: usize = 2 * MAX_CLIPBOARD_BYTES + 64;
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    const PNG: [u8; 12] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 255];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }

    fn image_bytes(clip: &Clip) -> Vec<u8> {
        let Clip::Image(input) = clip else {
            panic!("an image expected: {clip:?}");
        };
        assert_eq!(input.name.as_deref(), Some("clipboard"));
        let payload = input
            .uri
            .strip_prefix("data:image/png;base64,")
            .expect("png data URI");
        STANDARD.decode(payload).unwrap()
    }

    #[test]
    fn macos_tries_the_png_then_text() {
        let plan = read_plan("macos");
        let programs: Vec<_> = plan.iter().map(|c| (c.program, c.format)).collect();
        assert_eq!(
            programs,
            [("osascript", Format::PngHex), ("pbpaste", Format::Text)]
        );
    }

    #[test]
    fn linux_tries_both_image_tools_before_text() {
        let plan = read_plan("linux");
        let formats: Vec<_> = plan.iter().map(|c| c.format).collect();
        assert_eq!(
            formats,
            [
                Format::PngBytes,
                Format::PngBytes,
                Format::Text,
                Format::Text
            ]
        );
        let programs: Vec<_> = plan.iter().map(|c| c.program).collect();
        assert_eq!(programs, ["wl-paste", "xclip", "wl-paste", "xclip"]);
    }

    #[test]
    fn other_platforms_have_no_plan() {
        for os in ["windows", "freebsd", ""] {
            assert!(read_plan(os).is_empty(), "{os}");
        }
    }

    #[test]
    fn raw_png_bytes_become_an_attachment() {
        let clip = classify(Format::PngBytes, PNG.to_vec());
        assert_eq!(image_bytes(&clip), PNG);
    }

    #[test]
    fn bytes_without_the_png_magic_are_not_an_image() {
        assert_eq!(
            classify(Format::PngBytes, b"GIF89a...".to_vec()),
            Clip::Empty
        );
        assert_eq!(classify(Format::PngBytes, Vec::new()), Clip::Empty);
        assert_eq!(classify(Format::PngBytes, PNG[..4].to_vec()), Clip::Empty);
    }

    #[test]
    fn osascript_hex_text_decodes_to_the_png() {
        let out = format!("«data PNGf{}»\n", hex(&PNG));
        let clip = classify(Format::PngHex, out.into_bytes());
        assert_eq!(image_bytes(&clip), PNG);
    }

    #[test]
    fn malformed_osascript_output_is_not_an_image() {
        let odd = format!("«data PNGf{}A»", hex(&PNG));
        let bad_digit = format!("«data PNGf{}ZZ»", hex(&PNG));
        let not_png = format!("«data PNGf{}»", hex(b"hello world!"));
        for out in [
            odd.as_str(),
            bad_digit.as_str(),
            not_png.as_str(),
            "",
            "«data PNGf»",
            "missing the header",
        ] {
            assert_eq!(
                classify(Format::PngHex, out.as_bytes().to_vec()),
                Clip::Empty,
                "{out:?}"
            );
        }
    }

    #[test]
    fn text_is_kept_verbatim_and_empty_text_is_nothing() {
        assert_eq!(
            classify(Format::Text, "你好\nworld".as_bytes().to_vec()),
            Clip::Text("你好\nworld".into())
        );
        assert_eq!(classify(Format::Text, Vec::new()), Clip::Empty);
        assert_eq!(classify(Format::Text, vec![0xff, 0xfe]), Clip::Empty);
    }

    #[test]
    fn output_over_the_cap_is_dropped() {
        let mut big = PNG.to_vec();
        big.resize(MAX_CLIPBOARD_BYTES + 1, 0);
        assert_eq!(classify(Format::PngBytes, big), Clip::Empty);
        assert_eq!(
            classify(Format::Text, vec![b'a'; MAX_CLIPBOARD_BYTES + 1]),
            Clip::Empty
        );
    }

    fn sh(args: &'static [&'static str]) -> Cmd {
        Cmd {
            program: "sh",
            args,
            format: Format::Text,
        }
    }

    #[test]
    fn a_command_that_succeeds_returns_its_stdout() {
        let out = run_with_timeout(&sh(&["-c", "printf hi"]), Duration::from_secs(10));
        assert_eq!(out.as_deref(), Some(b"hi".as_slice()));
    }

    #[test]
    fn a_failing_or_missing_command_returns_none() {
        assert_eq!(
            run_with_timeout(&sh(&["-c", "printf hi; exit 3"]), Duration::from_secs(10)),
            None
        );
        let missing = Cmd {
            program: "ox-no-such-clipboard-tool",
            args: &[],
            format: Format::Text,
        };
        assert_eq!(run_with_timeout(&missing, Duration::from_secs(10)), None);
    }

    #[test]
    fn a_hung_command_is_killed_at_the_timeout() {
        let started = Instant::now();
        let out = run_with_timeout(&sh(&["-c", "sleep 30"]), Duration::from_millis(100));
        assert_eq!(out, None);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the child must be killed, not waited for"
        );
    }
}
