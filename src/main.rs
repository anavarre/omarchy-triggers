mod config;
mod keymap;
mod matcher;
mod output;
mod template;

use anyhow::{bail, Context, Result};
use config::Config;
use evdev::{Device, InputEventKind, Key};
use matcher::Buffer;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Forget what was typed after this long without a keystroke, since the user
/// has probably clicked somewhere else.
const IDLE_RESET: Duration = Duration::from_secs(5);

/// Expand anyway if the trigger key's release never shows up.
const RELEASE_WAIT: Duration = Duration::from_secs(1);

/// A new /dev/input node shows up before udev has given the input group access
/// to it, so opening it is retried for a while (20 x 100ms) on EACCES/ENOENT.
const OPEN_RETRIES: u32 = 20;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(100);

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

    let (rx, keyboards) = Keyboards::start();
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
                Err(TryRecvError::Disconnected) => bail!("event channel closed"),
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => (Key::KEY_RESERVED, -1),
                Err(RecvTimeoutError::Disconnected) => bail!("event channel closed"),
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
            // Resolved now, so a failure (empty clipboard...) leaves the typed
            // trigger alone instead of erasing it for nothing.
            match template::render(&s.snippet) {
                Ok(text) => {
                    pending = Some(Pending {
                        since: now,
                        trigger: s.trigger.clone(),
                        erase: s.trigger.chars().count(),
                        text,
                    });
                }
                Err(e) => eprintln!("triggers: not expanding {:?}: {e:#}", s.trigger),
            }
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

fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(Key::KEY_A) && keys.contains(Key::KEY_ENTER))
}

/// The keyboards currently open. They are read, and only grabbed while a snippet
/// is typed (see `expand`). Keyboards come and go: the ones present at startup
/// are picked up, then /dev/input is watched for new ones (Bluetooth reconnect,
/// suspend/resume, USB replug). A keyboard that disappears is dropped and the
/// rest keep working, with no restart.
struct Keyboards {
    tx: Sender<(Key, i32)>,
    devices: Mutex<Vec<Open>>,
}

struct Open {
    id: u64,
    path: PathBuf,
    /// A duplicate shares the device's grab state and stays valid even after the
    /// reader thread drops the device.
    fd: OwnedFd,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

impl Keyboards {
    /// Opens the keyboards present now, then watches for the ones that come later.
    /// Having none yet is fine: the service waits for one to show up.
    fn start() -> (Receiver<(Key, i32)>, Arc<Keyboards>) {
        let (tx, rx) = mpsc::channel();
        let kb = Arc::new(Keyboards { tx, devices: Mutex::new(Vec::new()) });

        // Watch first, scan second, so a keyboard appearing in between is seen
        // by one of the two (the other one skips the duplicate).
        if let Err(e) = kb.watch() {
            eprintln!("triggers: not watching for new keyboards: {e}");
        }
        let mut paths: Vec<PathBuf> = std::fs::read_dir("/dev/input")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_event_node(p.file_name().and_then(|n| n.to_str()).unwrap_or("")))
            .collect();
        paths.sort();
        for path in paths {
            kb.attach(&path, 0);
        }
        if kb.devices.lock().unwrap().is_empty() {
            eprintln!(
                "triggers: no readable keyboards in /dev/input yet. If one is plugged in, \
                 run triggers from the system service in contrib/, which grants it the \
                 input group. Waiting for one to appear"
            );
        }
        (rx, kb)
    }

    /// Opens `path` if it is a keyboard we don't have yet, and starts reading it.
    fn attach(self: &Arc<Self>, path: &Path, retries: u32) {
        let mut tries = 0;
        let device = loop {
            match Device::open(path) {
                Ok(d) => break d,
                Err(e)
                    if tries < retries
                        && matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::NotFound) =>
                {
                    tries += 1;
                    std::thread::sleep(OPEN_RETRY_DELAY);
                }
                Err(e) => {
                    if e.kind() != ErrorKind::NotFound {
                        eprintln!("triggers: can't open {}: {e}", path.display());
                    }
                    return;
                }
            }
        };
        if !is_keyboard(&device) {
            return;
        }
        // SAFETY: the device owns this fd and is alive here.
        let fd = unsafe { BorrowedFd::borrow_raw(device.as_raw_fd()) };
        let Ok(fd) = fd.try_clone_to_owned() else {
            eprintln!("triggers: can't duplicate fd of {}", path.display());
            return;
        };
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        {
            let mut devices = self.devices.lock().unwrap();
            if devices.iter().any(|d| d.path == path) {
                return;
            }
            devices.push(Open { id, path: path.to_owned(), fd });
        }
        eprintln!(
            "triggers: listening on {} ({})",
            path.display(),
            device.name().unwrap_or("unnamed")
        );

        let kb = Arc::clone(self);
        let path = path.to_owned();
        std::thread::spawn(move || {
            // Keys this device holds down, so that losing it mid-press doesn't
            // leave them looking stuck. Other keyboards are not affected.
            let mut held = HashSet::new();
            let mut device = device;
            'read: loop {
                let events = match device.fetch_events() {
                    Ok(events) => events,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                for ev in events {
                    if let InputEventKind::Key(key) = ev.kind() {
                        match ev.value() {
                            0 => drop(held.remove(&key)),
                            1 => drop(held.insert(key)),
                            _ => {}
                        }
                        if kb.tx.send((key, ev.value())).is_err() {
                            break 'read;
                        }
                    }
                }
            }
            eprintln!("triggers: lost {}", path.display());
            kb.devices.lock().unwrap().retain(|d| d.id != id);
            for key in held {
                let _ = kb.tx.send((key, 0));
            }
        });
    }

    /// Watches /dev/input for new device nodes with inotify. Removal needs no
    /// watch: reading a removed device fails, which ends its reader.
    fn watch(self: &Arc<Self>) -> std::io::Result<()> {
        // SAFETY: plain syscalls; the fd is wrapped in an OwnedFd right away.
        let fd = unsafe {
            let raw = libc::inotify_init1(libc::IN_CLOEXEC);
            if raw < 0 {
                return Err(std::io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(raw)
        };
        // SAFETY: the path is a valid NUL-terminated string.
        let wd = unsafe {
            libc::inotify_add_watch(
                fd.as_raw_fd(),
                c"/dev/input".as_ptr(),
                libc::IN_CREATE | libc::IN_MOVED_TO,
            )
        };
        if wd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let kb = Arc::clone(self);
        std::thread::spawn(move || {
            // u64 elements keep the buffer aligned for inotify_event.
            let mut buf = [0u64; 512];
            loop {
                // SAFETY: the buffer is valid for the length passed.
                let n = unsafe {
                    libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf))
                };
                if n < 0 {
                    if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    eprintln!("triggers: stopped watching for new keyboards");
                    return;
                }
                // SAFETY: the kernel filled the first n bytes with whole events.
                let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
                for name in inotify_names(bytes) {
                    if !is_event_node(&name) {
                        continue;
                    }
                    // Own thread: the retries for udev's permissions must not
                    // hold up other keyboards that appear at the same time.
                    let kb = Arc::clone(&kb);
                    std::thread::spawn(move || {
                        kb.attach(&Path::new("/dev/input").join(name), OPEN_RETRIES)
                    });
                }
            }
        });
        Ok(())
    }

    /// Grabs all keyboards, or none if any can't be (e.g. another program
    /// holds a grab): a key that slipped through would be typed twice.
    fn grab(&self) -> bool {
        let devices = self.devices.lock().unwrap();
        for (i, d) in devices.iter().enumerate() {
            if let Err(e) = set_grab(&d.fd, true) {
                eprintln!("triggers: can't grab keyboard: {e}");
                devices[..i].iter().for_each(|d| drop(set_grab(&d.fd, false)));
                return false;
            }
        }
        true
    }

    fn ungrab(&self) {
        for d in self.devices.lock().unwrap().iter() {
            // A keyboard unplugged meanwhile fails here, which is fine.
            let _ = set_grab(&d.fd, false);
        }
    }
}

fn is_event_node(name: &str) -> bool {
    name.strip_prefix("event").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// File names in a buffer of `struct inotify_event`s.
fn inotify_names(mut bytes: &[u8]) -> Vec<String> {
    const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
    let mut names = Vec::new();
    while bytes.len() >= HEADER {
        // SAFETY: at least HEADER bytes are available; read_unaligned has no
        // alignment requirement.
        let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast()) };
        let end = HEADER + ev.len as usize;
        if bytes.len() < end {
            break;
        }
        let raw = &bytes[HEADER..end];
        let raw = raw.split(|&b| b == 0).next().unwrap_or(&[]);
        if !raw.is_empty() {
            names.push(String::from_utf8_lossy(raw).into_owned());
        }
        bytes = &bytes[end..];
    }
    names
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_nodes() {
        assert!(is_event_node("event0"));
        assert!(is_event_node("event23"));
        assert!(!is_event_node("event"));
        assert!(!is_event_node("mouse0"));
        assert!(!is_event_node("by-id"));
    }

    #[test]
    fn parses_inotify_buffer() {
        let header = std::mem::size_of::<libc::inotify_event>();
        let mut buf = Vec::new();
        for name in ["event5", "event6"] {
            let len = 16; // padded, NUL-terminated, as the kernel does
            let mut ev = vec![0u8; header + len];
            ev[header - 4..header].copy_from_slice(&(len as u32).to_ne_bytes());
            ev[header..header + name.len()].copy_from_slice(name.as_bytes());
            buf.extend(ev);
        }
        assert_eq!(inotify_names(&buf), ["event5", "event6"]);
        // A truncated trailing event is ignored.
        assert_eq!(inotify_names(&buf[..buf.len() - 5]), ["event5"]);
    }
}
