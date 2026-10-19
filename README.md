# triggers

A minimal text expander for Wayland (built for Hyprland). Type a trigger such as
`rg` in any app and it is replaced with its snippet (`ripgrep`).

It reads keystrokes from `/dev/input` and types the replacement with `wtype`.
While a snippet is being typed, the keyboards are briefly grabbed so keys you
press meanwhile (say, Enter right after a password trigger) don't land in the
middle of it. They're typed right after the snippet instead. There's no GUI and no plugins.

## Config

`~/.config/triggers/config.toml` (or `--config PATH`):

```toml
[[snippets]]
trigger = "mmail"
snippet = "joe@localhost.com"

[[snippets]]
trigger = ";sig"
snippet = """
Best,
Joe"""
```

- Snippets can hold secrets (a password trigger, say), so keep the file
  readable by you alone (`chmod 600`); `triggers` warns otherwise. Snippet
  text is handed to `wtype` over stdin, never on its command line, so other
  users on the machine can't see it in `ps`.
- Triggers can't contain spaces.
- A trigger that starts with a letter or digit only fires at the start of a word,
  so `rg` won't fire inside `forget`. It still fires as you start typing `rgb`;
  use a punctuation prefix (`;rg`) if that gets in the way.
- If two triggers match, the longest one wins.
- Validate with `triggers check`. Reload with `sudo systemctl reload triggers`.

## Install

```sh
sudo pacman -S rustup wtype && rustup default stable
cargo install --path .
mkdir -p ~/.config/triggers && install -m 600 config.example.toml ~/.config/triggers/config.toml
sed -e "s|@USER@|$USER|" -e "s|@UID@|$(id -u)|" -e "s|@HOME@|$HOME|" \
    -e "s|@WAYLAND_DISPLAY@|$WAYLAND_DISPLAY|" contrib/triggers.service \
  | sudo tee /etc/systemd/system/triggers.service >/dev/null
sudo systemctl enable --now triggers
```

Run the `sed` line from inside your Hyprland session so `$WAYLAND_DISPLAY` is
set (usually `wayland-1`).

Logs: `journalctl -u triggers -f`

### Why a system service

Reading `/dev/input` takes the `input` group, which also lets a process inject
keys into the keyboards and grab them. Adding yourself to that group would hand
all of this to every program you run: your browser, a build script, anything.
Instead, the service runs as you but is the only process given the group, and
it's sandboxed:

- It opens the keyboards read-only, so it can read and grab them but not inject keys.
- It has no network, so even a compromised `triggers` can't send keystrokes anywhere.
- It's non-dumpable: your other processes can't attach to it or borrow its open
  keyboards through `/proc`, and it never writes a core dump.

If you were in the `input` group from an earlier install, leave it
(`sudo gpasswd -d $USER input`, then log out and back in) and remove the old
user service (`systemctl --user disable --now triggers`, then delete
`~/.config/systemd/user/triggers.service`).

## Limitations

- Keystrokes are decoded as a US layout.
- Keyboards plugged in after startup aren't picked up; restart the service.
- The service still runs as your user, so your own processes can read its
  config and could kill or replace it; the sandbox limits what `triggers`
  itself can do, not what can be done to it.
- The service starts at boot, not with your session, and assumes the Wayland
  socket name it was installed with. If that changes, re-run the `sed` line.
