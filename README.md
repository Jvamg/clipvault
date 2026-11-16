# ClipVault

Smart **clipboard manager and Time-Machine** written in Rust, featuring automatic semantic classification, terminal image capture and graphical preview, secret masking with auto-expiration, embedded local persistence, and real-time fuzzy search.

## Compatibility

`clipvault` works on **any Linux distribution (Wayland and X11)** — including Pop!_OS, Ubuntu, Debian, Fedora, Arch Linux, and derivatives — and also compiles natively on **macOS and Windows**, using `arboard` and embedded SQLite via `rusqlite` (`bundled`, with no runtime database dependencies).

- **Linux (Wayland)**: native support for persisting clipboard entries after exiting the TUI and capturing text/PNG images (`wl-clipboard` providing `wl-paste` and `wl-copy` is recommended).
- **Linux (X11)**: supported via `arboard` with automatic fallback to `xclip` or `xsel`.
- **macOS & Windows**: compiles directly with `cargo build --release` using `arboard` for clipboard capture and restoration.

---

## Key Features

- **Background Watcher (`daemon`)**: monitors system clipboard changes (~250ms polling), supports text and PNG images, and ignores self-generated echo events when restoring older entries.
- **Automatic Heuristic Classification**: categorizes each copied item into:
  - `[JSON]` — automatically generates an indented `pretty_json` view in the preview pane.
  - `[URL]` — `http`/`https`/`ftp`/`ws`/`wss`/`ssh` links.
  - `[CODE]` — source code snippets (Rust, JS/TS, Python, C/C++, etc.).
  - `[CMD]` — terminal commands, pipes, and shell scripts (`#!/bin/bash`, `cargo`, `git`, `docker`, etc.).
  - `[SECRET]` — tokens, API keys (`sk-...`, `ghp_...`, `AKIA...`, `AIza...`), `Bearer` tokens, JWTs, PEM/OpenSSH private keys, and explicit password assignments.
  - `[IMG]` — copied screenshots and PNG images (with terminal graphical preview via Sixel/Kitty/Halfblocks, restoration with `Enter`, and 1:1 external viewer opening with `v`).
  - `[TEXT]` — plain text (fallback).
- **Security by Default (`SecretToken`)**:
  - Automatically masked in both the list and preview panes (revealed on demand with `s`).
  - **Automatic 5-minute TTL (300 seconds)** purged by the background garbage collector and on database reads.
- **Fast Persistence & Deduplication**:
  - Embedded SQLite database configured with **WAL mode** (`journal_mode = WAL`, `busy_timeout = 5000`) for safe concurrent access between `daemon` and `ui`.
  - **SHA-256 deduplication**: copying identical content updates its timestamp, increments `use_count`, and preserves `pinned` status.
- **Interactive Terminal UI (`ratatui` + `crossterm`)**:
  - Real-time fuzzy search (`fuzzy-matcher`) with a split history list and formatted/graphical preview pane.

---

## Installation & Automatic Daemon (`systemd`)

### 1. Prerequisites

- Rust toolchain (`cargo` and `rustc`) installed via [rustup](https://rustup.rs/).
- On Linux, recommended clipboard utilities for your display server:
  - **Wayland**: `wl-clipboard` (`sudo apt install wl-clipboard` / `sudo dnf install wl-clipboard` / `sudo pacman -S wl-clipboard`)
  - **X11**: `xclip` or `xsel` plus standard XCB headers (on minimal Debian/Ubuntu installs: `sudo apt install xclip libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev`)

### 2. Clone, Build, and Install to `~/.local/bin`

```bash
git clone https://github.com/Jvamg/clipvault.git
cd clipvault
cargo build --release
mkdir -p ~/.local/bin
cp target/release/clipvault ~/.local/bin/clipvault
```

> Make sure `~/.local/bin` is included in your `$PATH` (or reference `~/.local/bin/clipvault` directly).

### 3. Enable the Automatic User Service (`systemd --user`)

Create `~/.config/systemd/user/clipvault.service` so the background watcher starts automatically on login:

```bash
mkdir -p ~/.config/systemd/user
cat << 'EOF' > ~/.config/systemd/user/clipvault.service
[Unit]
Description=ClipVault Clipboard Time-Machine Daemon
After=graphical-session.target

[Service]
ExecStart=%h/.local/bin/clipvault daemon
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
EOF
```

Reload `systemd`, enable, and start the service:

```bash
systemctl --user daemon-reload
systemctl --user enable --now clipvault.service
systemctl --user status clipvault.service
```

To follow live daemon logs:

```bash
journalctl --user -u clipvault.service -f
```

---

## Global Shortcut Setup (`Super + V` / `Win + V`)

To summon **ClipVault** from anywhere using `Super + V` (`Win + V`), add a custom keyboard shortcut in your desktop environment pointing to your preferred terminal emulator:

1. Open **System Settings** → **Keyboard** → **Custom Shortcuts** (or the equivalent section in your desktop/window manager).
2. Create a new shortcut:
   - **Name**: `ClipVault`
   - **Key combination**: `Super + V`
   - **Command** (choose the command matching your terminal):

- **Pop!_OS / COSMIC Terminal**:
  ```bash
  cosmic-term -e ~/.local/bin/clipvault
  ```
- **GNOME Terminal (Ubuntu / Fedora / Debian)**:
  ```bash
  gnome-terminal -- ~/.local/bin/clipvault
  ```
- **Kitty**:
  ```bash
  kitty ~/.local/bin/clipvault
  ```
- **Alacritty**:
  ```bash
  alacritty -e ~/.local/bin/clipvault
  ```

> **Tip:** On Wayland/COSMIC, if your desktop launcher does not expand `~`, use the full path (e.g. `/home/your-user/.local/bin/clipvault`) or wrap it with `sh -c '~/.local/bin/clipvault'`.

---

## Manual CLI Usage

### 1. Start the daemon manually

```bash
clipvault daemon
```

By default, the local database is stored at `~/.local/share/clipvault/clipvault.db`. Pass `--db` to override it:

```bash
clipvault --db /tmp/my-history.db daemon
```

### 2. Open the interactive TUI (`ui`)

```bash
clipvault
# or explicitly:
clipvault ui
```

---

## TUI Keybindings

| Key | Action |
| --- | --- |
| `Enter` | Copies the selected item (text or `[IMG]`) back to the clipboard and quits the TUI |
| `v` (or `Ctrl+V`) | Saves the selected `[IMG]` to `/tmp/clipvault-preview.png` and opens it in the default system viewer (`xdg-open`) |
| `↑` / `↓` or `j` / `k` | Navigates through history (re-masks secrets automatically when changing selection) |
| `/` or `Tab` | Toggles focus on the real-time fuzzy search bar |
| `p` (or `Ctrl+P`) | Pins or unpins the selected entry at the top of the list |
| `d` (or `Ctrl+D` / `Delete`) | Deletes the selected entry from history |
| `s` (or `Ctrl+S`) | Reveals or hides a `SecretToken` in the preview pane |
| `Backspace` | Removes the last character from the search query |
| `Esc` | Clears the active search filter or exits the TUI |

---

## Architecture (`src/`)

- `src/domain.rs` — Core models (`ClipEntry`, `ContentKind`), SHA-256 hashing, safe preview masking, and relative time formatting.
- `src/classifier.rs` — Heuristic classifier powered by `regex` and `serde_json` prioritizing security for `SecretToken` entries.
- `src/storage.rs` — Embedded SQLite persistence with `rusqlite` (WAL mode), CRUD, hash deduplication, PNG blob storage, `last_restored_hash` tracking, expiration purging, and fuzzy search.
- `src/watcher.rs` — Background clipboard monitor via `arboard` + Wayland/X11 CLI support (`wl-paste`/`wl-copy`, `xclip`/`xsel`) over an `std::sync::mpsc` channel with anti-echo filtering and periodic GC.
- `src/tui.rs` — Interactive terminal interface built with `ratatui`, `crossterm`, and `ratatui-image` (Sixel/Kitty/Halfblocks).
- `src/main.rs` — `clap` CLI supporting `daemon` and `ui` (default) subcommands.

---

## Running Tests

Run the full unit test suite with:

```bash
cargo test
```

---

### Quick Install via Cargo (`crates.io`)

    cargo install clipvault-tui

*(The binary is installed at `~/.cargo/bin/clipvault` — remember to point `ExecStart=` in your `systemd` unit to `
---

### Quick Install via Cargo (`crates.io`)

    cargo install clipvault-tui

*(The binary is installed at `~/.cargo/bin/clipvault` — remember to point `ExecStart=` in your `systemd` unit to `%h/.cargo/bin/clipvault daemon` if you install via Cargo.)*

### Troubleshooting (Wayland Boot Race Condition)

If `clipvault` stops capturing items right after a system reboot, the user service likely started before the Wayland compositor exported `WAYLAND_DISPLAY`. Ensure your unit file includes `After=graphical-session.target`, `PartOf=graphical-session.target`, and `ExecStartPre=/bin/sleep 3`, or re-import the session environment manually:

    systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR && systemctl --user restart clipvault.service
