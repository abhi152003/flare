<p align="center">
  <img width="200" alt="Flare Logo" src="./extra/logo/compat/Flare-1.png">
</p>

<h1 align="center">Flare</h1>

<p align="center">
  A fast, aesthetic terminal emulator forked from Alacritty.
</p>

## About

Flare is a custom fork of Alacritty focused on a more expressive desktop experience while keeping
Alacritty's rendering and terminal core.

Current Flare-specific work includes:

- runtime theme switching
- runtime opacity updates
- custom tab bar
- split panes
- custom desktop metadata and packaging

## Installation

### Run From Source

```sh
cd alacritty
cargo build --release -p flare
./target/release/flare
```

### Install Locally With Cargo

```sh
cd alacritty
cargo install --path alacritty --locked --force
flare
```

### Build A Debian Package

```sh
cd alacritty
cargo install cargo-deb
cargo deb -p flare
```

The generated package will be placed in `target/debian/`.

## Runtime Configuration

Flare supports runtime configuration through IPC:

```sh
flare msg config 'window.opacity=0.82'
flare msg config 'window.theme_preset="tokyo-night"'
flare msg config 'window.theme_preset="catppuccin-mocha"'
flare msg config 'window.theme_preset="nord"'
flare msg config 'window.theme_preset="dracula"'
flare msg config 'window.theme_preset="one-dark"'
```

Runtime config changes are persisted to a runtime override file, so they survive restarting Flare.

To clear runtime overrides:

```sh
flare msg config --reset
```

## Theme Presets

Supported built-in presets:

- `tokyo-night`
- `catppuccin-mocha`
- `nord`
- `dracula`
- `one-dark`

## Configuration File

Flare currently follows Alacritty's config file locations and TOML structure.

Typical config paths on Linux:

1. `$XDG_CONFIG_HOME/alacritty/alacritty.toml`
2. `$XDG_CONFIG_HOME/alacritty.toml`
3. `$HOME/.config/alacritty/alacritty.toml`
4. `$HOME/.alacritty.toml`
5. `/etc/alacritty/alacritty.toml`

## Shell Integration

Flare tracks each pane's working directory so that new tabs, split panes, and session restore open
in the right folder. For best results, add a few lines to your shell config so it reports its
directory to Flare via the OSC 7 escape sequence. Without this Flare still works (it falls back to
reading the process directory), but the shell-reported value is more reliable — especially at quit
time.

**zsh** (`~/.zshrc`):
```sh
autoload -Uz add-zsh-hook
add-zsh-hook precmd _flare_report_cwd
_flare_report_cwd() { printf '\e]7;%s\a' "file://$HOST$PWD" }
```

**bash** (`~/.bashrc`):
```sh
PROMPT_COMMAND='printf "\\e]7;file://%s%s\\a" "$HOSTNAME" "$PWD"'
```

**fish** — fish already emits OSC 7 in many setups; if not, add to `~/.config/fish/config.fish`:
```fish
function _flare_report_cwd --on-variable PWD
    printf '\e]7;file://%s%s\a' (hostname) "$PWD"
end
```

## Repository

- Homepage: <https://github.com/abhi152003/flare>
- Issues: <https://github.com/abhi152003/flare/issues>

## License

Flare inherits Alacritty's Apache-2.0 licensing in this fork.
