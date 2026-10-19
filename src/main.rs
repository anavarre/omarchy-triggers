mod config;
mod keymap;
mod matcher;
mod output;

use anyhow::{bail, Context, Result};
use config::Config;
use evdev::{Device, InputEventKind, Key};
use matcher::Buffer;
use std::collections::HashSet;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Forget what was typed after this long without a keystroke, since the user
/// has probably clicked somewhere else.
const IDLE_RESET: Duration = Duration::from_secs(5);

/// Expand anyway if the trigger key's release never shows up.
const RELEASE_WAIT: Duration = Duration::from_secs(1);

const USAGE: &str = "usage: triggers [--config PATH] [check]

  (no command)  run the expander
  check         validate the config and list its triggers

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

    // This process holds every keystroke and the snippets (maybe passwords).
    // Non-dumpable, other processes of the same user can't ptrace it or reach
    // its open keyboards through /proc, and it never writes a core dump.
    // SAFETY: PR_SET_DUMPABLE takes its argument by value.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } < 0 {
        bail!("can't make the process non-dumpable: {}", std::io::Error::last_os_error());
    }

    let mut config = Config::load(&config_path)?;
    if check {
        // Snippets may be passwords, so only their size is shown.
        for s in &config.snippets {
            println!("{} -> {} char(s)", s.trigger, s.snippet.chars().count());
        }
        println!("{} snippet(s) OK in {}", config.snippets.len(), config_path.display());
        return Ok(());
    }

    let reload = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGHUP, reload.clone())?;

    let (rx, keyboards) = spawn_keyboard_readers()?;
    eprintln!(
        "triggers: {} snippet(s) loaded from {}",
        config.snippets.len(),
        config_path.display()
    );

    let mut state = State::default();
    let mut last_key = Instant::now();
    let mut pending: Option<Pending> = None;

    loop {
        // Typing while the trigger key (or any other) is still held gets keys
        // dropped or mangled, so the expansion waits for everything to be let go
        // and for every queued event to be handled.
        let ready = pending
            .as_ref()
            .is_some_and(|p| state.held.is_empty() || p.since.elapsed() > RELEASE_WAIT);
        let (key, value) = if ready {
            match rx.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => {
                    expand(pending.take().unwrap(), &mut state, &keyboards, &rx);
                    continue;
                }
                Err(TryRecvError::Disconnected) => bail!("all keyboards disconnected"),
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => (Key::KEY_RESERVED, -1),
                Err(RecvTimeoutError::Disconnected) => bail!("all keyboards disconnected"),
            }
        };

        if reload.swap(false, Ordering::Relaxed) {
            match Config::load(&config_path) {
                Ok(c) => {
                    eprintln!("triggers: reloaded {} snippet(s)", c.snippets.len());
                    config = c;
                }
                Err(e) => eprintln!("triggers: keeping old config: {e:#}"),
            }
            state.buffer.clear();
        }

        // value: 0 = release, 1 = press, 2 = autorepeat
        let is_mod = state.track(key, value);
        if value <= 0 || is_mod {
            continue;
        }

        let now = Instant::now();
        if now.duration_since(last_key) > IDLE_RESET {
            state.buffer.clear();
        }
        last_key = now;

        let c = state.char_for(key);

        // Keys rolled over before the trigger key came up already reached the
        // app: erase them too and retype them after the snippet.
        if let Some(p) = &mut pending {
            match c {
                Some(c) => {
                    p.erase += 1;
                    p.text.push(c);
                    continue;
                }
                None => expand(pending.take().unwrap(), &mut state, &keyboards, &rx),
            }
        }

        if state.mods.chord() {
            state.buffer.clear();
            continue;
        }
        if key == Key::KEY_BACKSPACE {
            state.buffer.pop();
            continue;
        }
        let Some(c) = c else {
            // Enter, Tab, arrows, etc. move the cursor or leave the field.
            state.buffer.clear();
            continue;
        };

        state.buffer.push(c);
        if let Some(s) = state.buffer.find(&config.snippets) {
            state.buffer.clear();
            pending = Some(Pending {
                since: now,
                trigger: s.trigger.clone(),
                erase: s.trigger.chars().count(),
                text: s.snippet.clone(),
            });
        }
    }
}

/// Types the snippet. Keys are only read, never grabbed, so one pressed while
/// wtype is typing (like Enter right after a password trigger, where there's
/// nothing to watch) would land mid-snippet. So when no key is held, the
/// keyboards are grabbed while typing and whatever was pressed meanwhile is
/// replayed after. A held key is never grabbed: the compositor would miss its
/// release and repeat it.
fn expand(p: Pending, state: &mut State, keyboards: &Keyboards, rx: &Receiver<(Key, i32)>) {
    let grabbed = state.held.is_empty() && keyboards.grab();
    if let Err(e) = output::replace(p.erase, &p.text) {
        eprintln!("triggers: expanding {:?}: {e:#}", p.trigger);
    }
    if !grabbed {
        return;
    }
    // Hold the grab until keys pressed meanwhile are released too, so the
    // compositor never sees half a keystroke.
    let since = Instant::now();
    loop {
        let mut ops = Vec::new();
        while let Ok((key, value)) = rx.try_recv() {
            ops.extend(state.replayed(key, value));
        }
        if !ops.is_empty() {
            if let Err(e) = output::replay(&ops) {
                eprintln!("triggers: replaying keys: {e:#}");
            }
        } else if state.held.is_empty() || since.elapsed() > RELEASE_WAIT {
            break;
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    keyboards.ungrab();
}

/// Opens every keyboard and forwards its key events to one channel. Devices are
/// read, and only grabbed while a snippet is typed (see `expand`).
fn spawn_keyboard_readers() -> Result<(Receiver<(Key, i32)>, Keyboards)> {
    let (tx, rx) = mpsc::channel();
    let mut fds = Vec::new();
    for (path, mut device) in evdev::enumerate() {
        if !is_keyboard(&device) {
            continue;
        }
        eprintln!(
            "triggers: listening on {} ({})",
            path.display(),
            device.name().unwrap_or("unnamed")
        );
        // A duplicate shares the device's grab state and stays valid even after
        // the reader thread drops the device.
        // SAFETY: the device owns this fd and is alive here.
        let fd = unsafe { BorrowedFd::borrow_raw(device.as_raw_fd()) };
        fds.push(fd.try_clone_to_owned().context("duplicating keyboard fd")?);
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
    if fds.is_empty() {
        bail!(
            "no readable keyboards in /dev/input. Run triggers from the system \
             service in contrib/, which grants it the input group"
        );
    }
    Ok((rx, Keyboards { fds }))
}

fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(Key::KEY_A) && keys.contains(Key::KEY_ENTER))
}

/// Handles for grabbing every keyboard, so their keys reach only us.
struct Keyboards {
    fds: Vec<OwnedFd>,
}

impl Keyboards {
    /// Grabs all keyboards, or none if any can't be (e.g. another program
    /// holds a grab): a key that slipped through would be typed twice.
    fn grab(&self) -> bool {
        for (i, fd) in self.fds.iter().enumerate() {
            if let Err(e) = set_grab(fd, true) {
                eprintln!("triggers: can't grab keyboard: {e}");
                self.fds[..i].iter().for_each(|fd| drop(set_grab(fd, false)));
                return false;
            }
        }
        true
    }

    fn ungrab(&self) {
        for fd in &self.fds {
            // A keyboard unplugged meanwhile fails here, which is fine.
            let _ = set_grab(fd, false);
        }
    }
}

fn set_grab(fd: &OwnedFd, on: bool) -> std::io::Result<()> {
    // _IOW('E', 0x90, int)
    const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
    // SAFETY: EVIOCGRAB takes its argument by value; fd is an open evdev device.
    if unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGRAB as _, on as libc::c_int) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A matched trigger waiting for its key to be released.
struct Pending {
    since: Instant,
    trigger: String,
    erase: usize,
    text: String,
}

#[derive(Default)]
struct State {
    buffer: Buffer,
    mods: Modifiers,
    held: HashSet<Key>,
}

impl State {
    /// Tracks held keys and modifiers; returns true if `key` was a modifier.
    fn track(&mut self, key: Key, value: i32) -> bool {
        match value {
            0 => {
                self.held.remove(&key);
            }
            1 => {
                self.held.insert(key);
            }
            _ => {}
        }
        value >= 0 && self.mods.update(key, value)
    }

    fn char_for(&self, key: Key) -> Option<char> {
        if self.mods.chord() || key == Key::KEY_BACKSPACE {
            None
        } else {
            keymap::to_char(key, self.mods.shift(), self.mods.caps_lock)
        }
    }

    /// Handles a key pressed during a grab, returning how to replay it.
    fn replayed(&mut self, key: Key, value: i32) -> Option<output::Op> {
        if self.track(key, value) || value <= 0 {
            return None;
        }
        if let Some(c) = self.char_for(key) {
            self.buffer.push(c);
            return Some(output::Op::Text(c));
        }
        if key == Key::KEY_BACKSPACE && !self.mods.chord() {
            self.buffer.pop();
        } else {
            self.buffer.clear();
        }
        let Some(sym) = keymap::keysym(key) else {
            eprintln!("triggers: dropped {key:?} pressed during an expansion");
            return None;
        };
        Some(output::Op::Key { mods: self.mods.names(), sym })
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

    fn chord(&self) -> bool {
        self.ctrl != 0 || self.alt != 0 || self.meta != 0
    }

    /// wtype modifier names for what's held.
    fn names(&self) -> Vec<&'static str> {
        [(self.shift(), "shift"), (self.ctrl != 0, "ctrl"), (self.alt != 0, "alt"), (self.meta != 0, "logo")]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect()
    }
}
