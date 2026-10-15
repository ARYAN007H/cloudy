# CloudDrive

A multi-account Google Drive client for Linux that pools the free 15 GB storage of up to 4 personal Google accounts. Provides both a FUSE virtual filesystem (mount as a folder) and a native desktop GUI.

## Features
- **Pooled storage**: Merge multiple Google Drive accounts into one unified view
- **FUSE mount**: Access your files from any file manager or terminal
- **Native GUI**: Browse, upload, download, and preview files
- **Streaming reads**: Files are streamed on demand, not fully downloaded
- **Offline cache**: Recently accessed files are cached locally for fast repeat access
- **Secure**: OAuth tokens stored in system keyring, never in plaintext
- **Resumable uploads**: Interrupted uploads resume from where they left off

## System Requirements
- Arch Linux (tested) or other Linux distribution
- Rust stable toolchain (1.75+)
- System packages:
  ```bash
  sudo pacman -S fuse3 pkg-config libsecret webkit2gtk-4.1 base-devel
  ```
- A keyring daemon (gnome-keyring or kwallet)

## Build
```bash
# Clone and build
git clone https://github.com/ARYAN007H/cloudy.git
cd cloudy
cargo build --release

# Install Tauri CLI (for GUI development)
cargo install tauri-cli --version '^2'
```

Binaries are output to `target/release/`:
- `clouddrive-fuse` — FUSE filesystem mount
- `clouddrive-gui` — Desktop GUI application

## First-Time Setup
See [SETUP.md](SETUP.md) for a step-by-step guide to:
1. Create Google Cloud project and OAuth credentials
2. Configure the app with your Client ID and Secret
3. Add your Google accounts

## Usage

### FUSE Mount
```bash
# Mount (default: ~/CloudDrive)
./target/release/clouddrive-fuse

# Mount at custom location
./target/release/clouddrive-fuse --mount-point /mnt/gdrive

# Run in foreground (for debugging)
./target/release/clouddrive-fuse --foreground --log-level debug

# Unmount
fusermount3 -u ~/CloudDrive
# or
umount ~/CloudDrive
```

Mount point structure:
```
~/CloudDrive/
├── Pool/                  # Merged view across all accounts
└── Accounts/
    ├── user1@gmail.com/   # Per-account view
    ├── user2@gmail.com/
    └── ...
```

### Desktop GUI
```bash
./target/release/clouddrive-gui
```

## Architecture
Single Rust workspace with three crates:
- `core/` — Shared engine: OAuth, Drive API, SQLite cache, LRU content cache, sync
- `fuse-mount/` — FUSE filesystem binary (depends on core)
- `gui/` — Tauri desktop app (depends on core)

## Configuration
Config file: `~/.config/clouddrive/config.toml`
Metadata cache: `~/.local/share/clouddrive/clouddrive.db`
Content cache: `~/.cache/clouddrive/`
Logs: `~/.local/share/clouddrive/logs/`

## Known Limitations
- Google Workspace files (Docs, Sheets, Slides) are read-only and exported as .docx/.xlsx/.pptx
- Multi-parent files use the first parent as canonical location
- Testing mode tokens expire after 7 days (automatic re-auth prompt)
- Duplicate filenames are disambiguated with a Drive ID suffix

## License
MIT
