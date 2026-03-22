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

## Repository

- Homepage: <https://github.com/abhi152003/flare>
- Issues: <https://github.com/abhi152003/flare/issues>

## License

Flare inherits Alacritty's Apache-2.0 licensing in this fork.
