mod config;
mod keymap;
mod matcher;
mod output;

use anyhow::{bail, Context, Result};
use config::Config;
use evdev::{Device, InputEventKind, Key};
use matcher::Buffer;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Forget what was typed after this long without a keystroke, since the user
/// has probably clicked somewhere else.
const IDLE_RESET: Duration = Duration::from_secs(5);

/// Expand anyway if the trigger key's release never shows up.
const RELEASE_WAIT: Duration = Duration::from_secs(1);

const USAGE: &str = "usage: triggers [--config PATH] [check]

  (no command)  run the expander
  check         validate the config and list its snippets

Config defaults to $XDG_CONFIG_HOME/triggers/config.toml.
Send SIGHUP to reload it.";

fn main() {
    if let Err(e) = run() {
        eprintln!("triggers: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut config_path = config::default_path();
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                config_path = PathBuf::from(args.next().context("--config needs a path")?)
            }
            "check" => check = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => bail!("unknown argument {arg:?}\n\n{USAGE}"),
        }
    }

    let mut config = Config::load(&config_path)?;
    if check {
        for s in &config.snippets {
            println!("{} -> {:?}", s.trigger, s.snippet);
        }
        println!("{} snippet(s) OK in {}", config.snippets.len(), config_path.display());
        return Ok(());
    }

    let reload = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGHUP, reload.clone())?;

    let rx = spawn_keyboard_readers()?;
    eprintln!(
        "triggers: {} snippet(s) loaded from {}",
        config.snippets.len(),
        config_path.display()
    );

    let mut buffer = Buffer::default();
    let mut mods = Modifiers::default();
    let mut last_key = Instant::now();
    let mut pending: Option<Pending> = None;

    loop {
        let (key, value) = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => (Key::KEY_RESERVED, -1),
            Err(RecvTimeoutError::Disconnected) => bail!("all keyboards disconnected"),
        };

        if reload.swap(false, Ordering::Relaxed) {
            match Config::load(&config_path) {
                Ok(c) => {
                    eprintln!("triggers: reloaded {} snippet(s)", c.snippets.len());
                    config = c;
                }
                Err(e) => eprintln!("triggers: keeping old config: {e:#}"),
            }
            buffer.clear();
        }

        // value: 0 = release, 1 = press, 2 = autorepeat
        let is_mod = value >= 0 && mods.update(key, value);

        // Typing while the trigger key (or a modifier) is still held gets keys
        // dropped or mangled, so the expansion waits for everything to be let go.
        if let Some(p) = &mut pending {
            if key == p.key && value == 0 {
                p.key_up = true;
            }
            if (p.key_up && !mods.held()) || p.since.elapsed() > RELEASE_WAIT {
                pending.take().unwrap().expand();
            }
        }

        if value <= 0 || is_mod {
            continue;
        }

        let now = Instant::now();
        if now.duration_since(last_key) > IDLE_RESET {
            buffer.clear();
        }
        last_key = now;

        let c = if mods.chord() || key == Key::KEY_BACKSPACE {
            None
        } else {
            keymap::to_char(key, mods.shift(), mods.caps_lock)
        };

        // Keys rolled over before the trigger key came up already reached the
        // app: erase them too and retype them after the snippet.
        if let Some(p) = &mut pending {
            match c {
                Some(c) => {
                    p.erase += 1;
                    p.text.push(c);
                    continue;
                }
                None => pending.take().unwrap().expand(),
            }
        }

        if mods.chord() {
            buffer.clear();
            continue;
        }
        if key == Key::KEY_BACKSPACE {
            buffer.pop();
            continue;
        }
        let Some(c) = c else {
            // Enter, Tab, arrows, etc. move the cursor or leave the field.
            buffer.clear();
            continue;
        };

        buffer.push(c);
        if let Some(s) = buffer.find(&config.snippets) {
            buffer.clear();
            pending = Some(Pending {
                key,
                key_up: false,
                since: now,
                trigger: s.trigger.clone(),
                erase: s.trigger.chars().count(),
                text: s.snippet.clone(),
            });
        }
    }
}

/// Opens every keyboard and forwards its key events to one channel. Devices are
/// read, never grabbed, so typing always reaches the focused app untouched.
fn spawn_keyboard_readers() -> Result<mpsc::Receiver<(Key, i32)>> {
    let (tx, rx) = mpsc::channel();
    let mut count = 0;
    for (path, mut device) in evdev::enumerate() {
        if !is_keyboard(&device) {
            continue;
        }
        eprintln!(
            "triggers: listening on {} ({})",
            path.display(),
            device.name().unwrap_or("unnamed")
        );
        count += 1;
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            let Ok(events) = device.fetch_events() else {
                eprintln!("triggers: lost {}", path.display());
                return;
            };
            for ev in events {
                if let InputEventKind::Key(key) = ev.kind() {
                    if tx.send((key, ev.value())).is_err() {
                        return;
                    }
                }
            }
        });
    }
    if count == 0 {
        bail!(
            "no readable keyboards in /dev/input. Add yourself to the input group \
             (sudo usermod -aG input $USER) and log in again"
        );
    }
    Ok(rx)
}

fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(Key::KEY_A) && keys.contains(Key::KEY_ENTER))
}

/// A matched trigger waiting for its key to be released.
struct Pending {
    key: Key,
    key_up: bool,
    since: Instant,
    trigger: String,
    erase: usize,
    text: String,
}

impl Pending {
    fn expand(self) {
        if let Err(e) = output::replace(self.erase, &self.text) {
            eprintln!("triggers: expanding {:?}: {e:#}", self.trigger);
        }
    }
}

#[derive(Default)]
struct Modifiers {
    left_shift: bool,
    right_shift: bool,
    ctrl: u8,
    alt: u8,
    meta: u8,
    caps_lock: bool,
}

impl Modifiers {
    /// Tracks modifier state; returns true if `key` was a modifier.
    fn update(&mut self, key: Key, value: i32) -> bool {
        let down = value != 0;
        let set = |bit: &mut u8, mask: u8| {
            if down { *bit |= mask } else { *bit &= !mask }
        };
        match key {
            Key::KEY_LEFTSHIFT => self.left_shift = down,
            Key::KEY_RIGHTSHIFT => self.right_shift = down,
            Key::KEY_LEFTCTRL => set(&mut self.ctrl, 1),
            Key::KEY_RIGHTCTRL => set(&mut self.ctrl, 2),
            Key::KEY_LEFTALT => set(&mut self.alt, 1),
            Key::KEY_RIGHTALT => set(&mut self.alt, 2),
            Key::KEY_LEFTMETA => set(&mut self.meta, 1),
            Key::KEY_RIGHTMETA => set(&mut self.meta, 2),
            Key::KEY_CAPSLOCK => {
                if value == 1 {
                    self.caps_lock = !self.caps_lock;
                }
            }
            _ => return false,
        }
        true
    }

    fn shift(&self) -> bool {
        self.left_shift || self.right_shift
    }

    fn held(&self) -> bool {
        self.shift() || self.chord()
    }

    fn chord(&self) -> bool {
        self.ctrl != 0 || self.alt != 0 || self.meta != 0
    }
}
