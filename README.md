# Cloudy

Cloudy pools the free **15 GB** that every Google account comes with into **one big drive**.
Link as many accounts as you like, and browse, upload, download and preview everything from a
single place – the pool shows the union of all your accounts, and new files are placed in the
account that has room.

```
 you@gmail.com  ─┐
 spare1@gmail.com ├─►  Cloudy  ─►  one drive, 15 GB × N
 spare2@gmail.com ─┘
```

## Features

- **One pool, many accounts** – same-named folders from different accounts are merged into one
  folder; each file still lives whole in a single account (nothing is split or re-encoded).
- **Smart placement** – new uploads go to the account with the most free space (or round-robin,
  or a pinned account). Uploading into a pooled folder creates the matching folder in whichever
  account is chosen.
- **Fast, live UI** – the index is cached locally so the app opens instantly, even offline; changes
  stream in from Google's change feed. First-time indexing shows files as they are discovered.
- **Photos** view with thumbnails, month grouping, full-screen viewer and video streaming.
- **Uploads & downloads** with progress, cancel/retry, resumable uploads and folder support
  (drag & drop from your desktop works too).
- **Trash view** – Drive's trash still counts against your quota; Cloudy shows it so you can free space.
- **Secure** – refresh tokens live in the system keyring (a private `0600` file is the fallback);
  Cloudy talks only to Google, using *your own* OAuth client.
- **FUSE mount** (optional) – `clouddrive-fuse` exposes the same pool as a normal folder.

## Install

Requirements: Linux, Rust stable (1.80+), and on Arch:

```bash
sudo pacman -S --needed base-devel pkg-config fuse3 webkit2gtk-4.1 libsecret dbus
```
(Debian/Ubuntu: `libwebkit2gtk-4.1-dev libgtk-3-dev libfuse3-dev libdbus-1-dev libssl-dev pkg-config`.)

```bash
git clone https://github.com/ARYAN007H/cloudy.git && cd cloudy
cargo build --release
./target/release/clouddrive-gui       # desktop app
./target/release/clouddrive-fuse      # optional FUSE mount (see below)
```

## First run

Cloudy has a built-in setup wizard that walks you through creating free Google API credentials
(about 5 minutes, once) and then adding your first account. See [SETUP.md](SETUP.md) for the
details and for the one setting that decides whether sign-ins expire after 7 days.

## Things worth knowing

- **Storage is shared with Gmail and Photos.** A Google account's 15 GB covers Drive + Gmail + Google
  Photos, so "free space" is what is left of the whole allowance.
- **The biggest single file** you can add is the free space of your emptiest account (files are not
  split across accounts).
- **Moving files between accounts** isn't possible without copying (and temporarily doubling the
  data), so Cloudy refuses it with a clear message; moves within an account are instant.
- Files that are only *shared with you* are not part of your pool (they use someone else's quota).

## FUSE

```bash
./target/release/clouddrive-fuse                       # mounts at ~/CloudDrive
./target/release/clouddrive-fuse --mount-point /mnt/gdrive --foreground --log-level debug
fusermount3 -u ~/CloudDrive                            # unmount
```
The mount shows `pool/` (merged) and `accounts/<email>/` (exactly what each account contains).

## Where things are stored

| What | Location |
|------|----------|
| Settings | `~/.config/clouddrive/config.toml` |
| Linked accounts (e-mails only) | `~/.config/clouddrive/accounts.json` |
| Sign-ins & client secret | system keyring, else `~/.config/clouddrive/{tokens,credentials}.json` (mode 600) |
| File index | `~/.local/share/clouddrive/metadata.db` |
| Logs | `~/.local/share/clouddrive/logs/` |
| Content & thumbnail cache | `~/.cache/clouddrive/` |

Override with `CLOUDDRIVE_CONFIG_DIR`, `CLOUDDRIVE_DATA_DIR`, `CLOUDDRIVE_CACHE_DIR`.
Set `CLOUDY_LOG_STDERR=1` to also log to the terminal, and `CLOUDDRIVE_NO_KEYRING=1` to force the file backend.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| "Error 403: access_denied" when signing in | Add that Google account under *OAuth consent screen → Test users*, or publish the app. |
| An account says **Sign-in needed** after about a week | Your Google Cloud app is in *Testing* mode. Reconnect, and set the app to *In production* to stop it recurring. |
| "redirect_uri_mismatch" / "invalid_client" | Create the OAuth client as type **Desktop app**, and re-enter the Client ID/Secret in Settings. |
| "Google Drive API has not been used…" | Enable the Drive API for your project (SETUP.md, step 2). |
| Google says **"Something went wrong"** after *Continue* | Google-side error. Copy the sign-in link from Cloudy's dialog into a Chromium-based browser (Chrome/Brave/Edge); see [SETUP.md](SETUP.md#google-says-something-went-wrong-after-continue). |
| Browser doesn't open on sign-in | Use *Copy* in the dialog and paste the link into any browser on the same computer. |
| Photos/thumbnails missing for a file | Google hasn't generated a thumbnail for it (common for fresh uploads); it appears later. |

## Development

```bash
cargo test -p clouddrive-core      # unit tests + end-to-end tests against an in-process fake Google
cargo test -p clouddrive-gui       # loopback media-server tests
```
The core end-to-end test (`core/tests/e2e.rs`) drives the real engine against a fake OAuth + Drive
server: adding accounts, paginated sync, pooled folders, token refresh, chunked uploads, trash and
revoked sign-ins.

Layout: `core/` (engine: auth, Drive client, sync, tree, uploads), `gui/` (Tauri app + `frontend/`
HTML/CSS/JS), `fuse-mount/` (FUSE filesystem).
