use anyhow::{bail, Context, Result};
use std::process::Command;

const KEY_DELAY_MS: &str = "3";

/// Erases `erase` characters, then types `text`, through the Wayland
/// virtual-keyboard protocol. The text is passed after `--` so snippets
/// starting with `-` are never mistaken for flags. (Not over stdin: wtype 0.4
/// drops stdin text that follows `-k`.)
pub fn replace(erase: usize, text: &str) -> Result<()> {
    let mut cmd = Command::new("wtype");
    // Without a delay between keys, Hyprland drops some of them.
    cmd.args(["-d", KEY_DELAY_MS]);
    for _ in 0..erase {
        cmd.args(["-k", "BackSpace"]);
    }
    cmd.arg("--").arg(text);
    let status = cmd.status().context("running wtype (is it installed?)")?;
    if !status.success() {
        bail!("wtype exited with {status}");
    }
    Ok(())
}
