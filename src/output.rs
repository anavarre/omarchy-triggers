use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const KEY_DELAY_MS: &str = "3";

/// A key pressed while a snippet was being typed, to replay after it.
pub enum Op {
    Text(char),
    /// An xkb keysym name, with the modifiers held for it.
    Key { mods: Vec<&'static str>, sym: String },
}

/// Erases `erase` characters, then types `text`, through the Wayland
/// virtual-keyboard protocol. Text always goes over stdin, never on the command
/// line: snippets can hold passwords, and every user on the machine can read
/// another process's arguments in `ps` or /proc. wtype 0.4 drops stdin text
/// that follows `-k`, so the backspaces get their own call.
pub fn replace(erase: usize, text: &str) -> Result<()> {
    if erase > 0 {
        let args: Vec<&str> = std::iter::repeat_n(["-k", "BackSpace"], erase).flatten().collect();
        wtype(&args, None)?;
    }
    type_text(text)
}

/// Types `ops` in order. Each run of text gets its own call (see `replace`).
pub fn replay(ops: &[Op]) -> Result<()> {
    let mut args: Vec<String> = Vec::new();
    let mut text = String::new();
    for op in ops {
        match op {
            Op::Text(c) => text.push(*c),
            Op::Key { mods, sym } => {
                if !text.is_empty() {
                    type_text(&std::mem::take(&mut text))?;
                }
                mods.iter().for_each(|m| args.extend(["-M".into(), m.to_string()]));
                args.extend(["-k".into(), sym.clone()]);
                mods.iter().rev().for_each(|m| args.extend(["-m".into(), m.to_string()]));
            }
        }
    }
    if !args.is_empty() {
        wtype(&args, None)?;
    }
    type_text(&text)
}

fn type_text(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    wtype::<&str>(&[], Some(text))
}

/// Runs wtype with `args`, plus `stdin` as text to type if given.
fn wtype<S: AsRef<OsStr>>(args: &[S], stdin: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("wtype");
    // Without a delay between keys, Hyprland drops some of them.
    cmd.args(["-d", KEY_DELAY_MS]).args(args);
    if stdin.is_some() {
        cmd.arg("-").stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut child = cmd.spawn().context("running wtype (is it installed?)")?;
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().context("opening wtype's stdin")?;
        let text = text.to_owned();
        // Written from another thread so a text larger than the pipe buffer
        // can't block the timeout loop below; wtype types once it sees EOF.
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    // The keyboards may be grabbed meanwhile, so never wait on a stuck wtype.
    let chars: usize =
        args.iter().map(|a| a.as_ref().len()).sum::<usize>() + stdin.map_or(0, str::len);
    let deadline = Instant::now() + Duration::from_secs(2) + Duration::from_millis(20 * chars as u64);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("wtype exited with {status}");
            }
            return Ok(());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("wtype timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
