# triggers

A minimal text expander for Wayland (built for Hyprland). Type a trigger such as
`rg` in any app and it is replaced with its snippet (`ripgrep`).

It reads keystrokes from `/dev/input` (read-only, never grabbed) and types the
replacement with `wtype`. There's no GUI and no plugins.

## Config

`~/.config/triggers/config.toml` (or `--config PATH`):

```toml
[[snippets]]
trigger = "rg"
snippet = "ripgrep"

[[snippets]]
trigger = ";sig"
snippet = """
Best,
Aurelien"""
```

- Triggers can't contain spaces.
- A trigger that starts with a letter or digit only fires at the start of a word,
  so `rg` won't fire inside `forget`. It still fires as you start typing `rgb`;
  use a punctuation prefix (`;rg`) if that gets in the way.
- If two triggers match, the longest one wins.
- Validate with `triggers check`. Reload with `systemctl --user reload triggers`.

## Install

```sh
sudo pacman -S rustup wtype && rustup default stable
sudo usermod -aG input $USER     # then log out and back in
cargo install --path .
mkdir -p ~/.config/triggers && cp config.example.toml ~/.config/triggers/config.toml
mkdir -p ~/.config/systemd/user && cp contrib/triggers.service ~/.config/systemd/user/
systemctl --user enable --now triggers
```

Logs: `journalctl --user -u triggers -f`

## Limitations

- Keystrokes are decoded as a US layout.
- Keyboards plugged in after startup aren't picked up; restart the service.
- Being in the `input` group lets your user's processes read every keystroke.
  That's what makes this work, but it's a real security trade-off.
