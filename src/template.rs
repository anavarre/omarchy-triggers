//! Placeholders in snippets: `{{name}}` is replaced when the trigger fires, and
//! `{{{{` types a literal `{{`.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);
const CLIPBOARD_MAX_BYTES: u64 = 1 << 20;

#[derive(Debug, PartialEq)]
enum Part<'a> {
    Text(&'a str),
    Clipboard,
}

fn parse(snippet: &str) -> Result<Vec<Part<'_>>> {
    let mut parts = Vec::new();
    let mut rest = snippet;
    while let Some(i) = rest.find("{{") {
        if let Some(after) = rest[i..].strip_prefix("{{{{") {
            // Keep one `{{` as text and carry on after the escape.
            parts.push(Part::Text(&rest[..i + 2]));
            rest = after;
            continue;
        }
        if i > 0 {
            parts.push(Part::Text(&rest[..i]));
        }
        let tail = &rest[i + 2..];
        let Some(end) = tail.find("}}") else {
            bail!("unterminated `{{{{` in snippet (write `{{{{{{{{` for a literal one)");
        };
        match tail[..end].trim() {
            "clipboard" => parts.push(Part::Clipboard),
            other => bail!("unknown placeholder {{{{{other}}}}} (known: clipboard)"),
        }
        rest = &tail[end + 2..];
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest));
    }
    Ok(parts)
}

/// Checks that a snippet's placeholders are well formed and known.
pub fn validate(snippet: &str) -> Result<()> {
    parse(snippet).map(|_| ())
}

/// Replaces the placeholders in `snippet`. The clipboard is only read if used.
pub fn render(snippet: &str) -> Result<String> {
    render_with(snippet, clipboard)
}

fn render_with(snippet: &str, clip: impl Fn() -> Result<String>) -> Result<String> {
    let mut out = String::new();
    for part in parse(snippet)? {
        match part {
            Part::Text(t) => out.push_str(t),
            Part::Clipboard => out.push_str(&clip()?),
        }
    }
    Ok(out)
}

/// The latest clipboard text, via wl-paste (which only needs the Wayland socket).
fn clipboard() -> Result<String> {
    let mut child = Command::new("wl-paste")
        .args(["--no-newline", "--type", "text"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running wl-paste (is wl-clipboard installed?)")?;
    let stdout = child.stdout.take().context("opening wl-paste's stdout")?;
    // Read from another thread so a big clipboard can't fill the pipe and stall
    // the timeout loop below.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(CLIPBOARD_MAX_BYTES + 1).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("wl-paste timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let buf = reader.join().map_err(|_| anyhow::anyhow!("reading wl-paste panicked"))?;
    if !status.success() {
        bail!("wl-paste exited with {status} (clipboard empty or not text?)");
    }
    if buf.len() as u64 > CLIPBOARD_MAX_BYTES {
        bail!("clipboard is larger than {CLIPBOARD_MAX_BYTES} bytes");
    }
    String::from_utf8(buf).context("clipboard is not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Result<String> {
        render_with(s, || Ok("CLIP".to_string()))
    }

    #[test]
    fn substitutes_clipboard() {
        assert_eq!(r("a {{clipboard}} b {{ clipboard }}").unwrap(), "a CLIP b CLIP");
        assert_eq!(r("plain").unwrap(), "plain");
        assert_eq!(r("{{clipboard}}").unwrap(), "CLIP");
    }

    #[test]
    fn escapes_literal_braces() {
        assert_eq!(r("x {{{{clipboard}} y").unwrap(), "x {{clipboard}} y");
        assert_eq!(r("{{{{{{clipboard}}").unwrap(), "{{CLIP");
    }

    #[test]
    fn rejects_bad_placeholders() {
        assert!(validate("{{nope}}").is_err());
        assert!(validate("oops {{clipboard").is_err());
        assert!(validate("fine {{clipboard}}").is_ok());
    }

    #[test]
    fn clipboard_is_lazy() {
        let out = render_with("no placeholders", || bail!("must not be called")).unwrap();
        assert_eq!(out, "no placeholders");
    }

    #[test]
    fn clipboard_error_propagates() {
        assert!(render_with("{{clipboard}}", || bail!("empty")).is_err());
    }
}
