use anyhow::{bail, Context, Result};
use std::process::Command;
use std::time::{Duration, Instant};

const KEY_DELAY_MS: &str = "3";

/// A key pressed while a snippet was being typed, to replay after it.
pub enum Op {
    Text(char),
    /// An xkb keysym name, with the modifiers held for it.
    Key { mods: Vec<&'static str>, sym: String },
}

/// Erases `erase` characters, then types `text`, through the Wayland
/// virtual-keyboard protocol. The text is passed after `--` so snippets
/// starting with `-` are never mistaken for flags. (Not over stdin: wtype 0.4
/// drops stdin text that follows `-k`.)
pub fn replace(erase: usize, text: &str) -> Result<()> {
    let mut args = Vec::new();
    for _ in 0..erase {
        args.extend(["-k", "BackSpace"]);
    }
    args.extend(["--", text]);
    wtype(&args)
}

/// Types `ops` in order. Text goes last on a wtype command line (after `--`),
/// so each run of text gets its own call.
pub fn replay(ops: &[Op]) -> Result<()> {
    let mut args: Vec<String> = Vec::new();
    let mut text = String::new();
    for op in ops {
        match op {
            Op::Text(c) => text.push(*c),
            Op::Key { mods, sym } => {
                if !text.is_empty() {
                    args.extend(["--".into(), std::mem::take(&mut text)]);
                    wtype(&args)?;
                    args.clear();
                }
                mods.iter().for_each(|m| args.extend(["-M".into(), m.to_string()]));
                args.extend(["-k".into(), sym.clone()]);
                mods.iter().rev().for_each(|m| args.extend(["-m".into(), m.to_string()]));
            }
        }
    }
    if !text.is_empty() {
        args.extend(["--".into(), text]);
    }
    if args.is_empty() {
        return Ok(());
    }
    wtype(&args)
}

fn wtype<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Result<()> {
    let mut child = Command::new("wtype")
        // Without a delay between keys, Hyprland drops some of them.
        .args(["-d", KEY_DELAY_MS])
        .args(args)
        .spawn()
        .context("running wtype (is it installed?)")?;
    // The keyboards may be grabbed meanwhile, so never wait on a stuck wtype.
    let chars: usize = args.iter().map(|a| a.as_ref().len()).sum();
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
