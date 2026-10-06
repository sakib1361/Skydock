# Skydock

A cloud drive client for Linux. Skydock connects your OneDrive and Google Drive accounts to your desktop, with a small native window, a tray icon and a command-line tool.

It exists because Linux has no first-party OneDrive or Google Drive client, and the existing third-party ones tend to lack either files on demand, a desktop interface, or a simple `.deb` / AppImage install. Skydock is written in Rust, uses no Electron or Node.js, and runs without root.

> **Early development.** Skydock shows your drives as read-only folders whose files download when you open them. It cannot upload or change anything in the cloud yet. See [Features](#features) for exactly what works.

Not affiliated with or endorsed by Microsoft or Google. OneDrive and Google Drive are trademarks of their respective owners.

## Features

### Working today

- **OneDrive** (personal Microsoft accounts) and **Google Drive** (My Drive).
- **Browser sign-in.** You sign in on the provider's own page; Skydock never sees your password.
- **Sessions in the desktop keyring** (KWallet, GNOME Keyring), never in a plain file.
- **Change tracking.** The first run reads the whole drive's file list into a local database; later runs fetch only what changed.
- **Files on demand.** Each drive appears as a folder with every file at its real size, using no disk space. A file downloads the first time you open it and is checked against the provider's own hash; after that it opens instantly.
- **Stays current.** While the window or tray icon is running, drives are checked for changes every five minutes, or as often as you choose in Settings.
- **See and reclaim space.** Each drive shows how many files are downloaded to this device and how much room they take; Settings can free that space without deleting anything from the cloud.
- **Desktop window** showing each account, how much storage it uses, and how many files Skydock knows about, plus a **tray icon**.
- **Command-line tool** for the same operations.
- **Separate folder per provider**, so accounts never mix (see [Folder layout](#folder-layout)).
- **`.deb` and AppImage** packages from one script.

### Planned

- Writing: creating, editing, renaming and deleting files, uploaded to the cloud, with conflict handling.
- "Always keep on this device" and "Free up space".
- A background service, so the folders are there without the window running.
- File-manager integration for KDE Dolphin (status icons, right-click actions), then other desktops.
- Activity view and per-folder sync choices in the window.

### Limits to know about

- OneDrive work and school accounts are not supported.
- The folders are read-only for now, and exist only while Skydock is running.
- Google Docs, Sheets and Slides are listed as empty files that cannot be opened.
- On Ubuntu the sync folder must be inside your home folder, or under `/mnt` or `/media`; the system refuses on-demand folders elsewhere (for example on drives mounted under `/run/media`).
- Files that are only shared with you (not added to your own drive) are not included.

## Supported systems

Skydock needs a 64-bit Intel/AMD (x86_64) Linux with glibc 2.39 or newer, FUSE 3, and a desktop that provides a keyring. Only one system has actually been tested so far, so this list separates what is known from what is expected.

| System | Status |
|---|---|
| Kubuntu 26.04 (Plasma 6, Wayland) | **Tested.** This is the development machine. |
| Ubuntu 24.04, 24.10, 25.04, 25.10, 26.04 and their flavours (Kubuntu, Xubuntu, Lubuntu, Ubuntu MATE, Budgie) | **Should work, untested.** The packages are built to need nothing newer than Ubuntu 24.04 provides. Use the `.deb`. |
| Linux Mint 22 and newer, Pop!_OS 24.04, Zorin OS 18 and other Ubuntu 24.04-based systems | **Should work, untested.** Use the `.deb`. |
| Debian 13 "trixie" and newer | **Should work, untested.** Use the `.deb`. |
| Fedora 40 and newer, openSUSE Tumbleweed, Arch, Manjaro | **Probably works, untested.** Use the AppImage; `fuse3` must be installed. |
| Ubuntu 22.04 and older, Debian 12, Linux Mint 21 | **Not supported.** Their glibc is too old and the app will not start. |
| ARM (Raspberry Pi, ARM laptops) | **Not built.** Only x86_64 packages exist. |
| Alpine and other musl-based systems | **Not supported.** |

What differs by desktop rather than by distribution:

- **KDE Plasma** is the main target. Provider icons come from the Breeze icon theme.
- **GNOME** (default Ubuntu): the tray icon relies on AppIndicator support, which Ubuntu enables out of the box; plain GNOME on other distributions needs the AppIndicator extension. Without a tray, closing the window quits Skydock. Icon themes without OneDrive and Google Drive icons show a plain cloud instead.
- **Cinnamon, XFCE, MATE, LXQt**: expected to work the same way as GNOME with AppIndicator support.
- **Sync folder location**: Ubuntu and its derivatives only allow on-demand folders inside your home folder, or under `/mnt` or `/media`.

If you try Skydock on a system marked untested, an issue saying whether it worked is very welcome.

## Folder layout

You choose one sync folder; it defaults to your home directory. Each provider gets its own folder inside it:

```
<sync folder>/OneDrive
<sync folder>/Google Drive
```

## Install

There are no published releases yet. Check [Supported systems](#supported-systems), then build from source:

```sh
sudo apt install build-essential pkg-config libsqlite3-dev libfontconfig-dev fuse3
# Rust: https://rustup.rs
cargo build --release
```

Or build packages (see [Packaging](#packaging)) and install the `.deb`:

```sh
scripts/package.sh deb
sudo apt install ./out/skydock_*.deb
```

## Configure the provider client IDs

Skydock needs to be registered as an application with Microsoft and with Google before either will let it start a sign-in. The registration gives a **client ID**, which identifies the app, not you. Release builds will include these; until then, anyone building Skydock creates their own. Each takes about ten minutes and is free.

Menu names in both consoles change from time to time, so the wording below may differ slightly from what you see.

### OneDrive (Microsoft)

1. Open <https://entra.microsoft.com> and sign in with your Microsoft account.
2. Go to **App registrations → New registration**.
   - If you see *"The ability to create applications outside of a directory has been deprecated"*, your account has no directory yet. Follow the **Signing up for Azure** link in that message and complete the free sign-up (it asks for a card to verify identity; app registrations cost nothing). Then return to this step.
3. Fill in the form:
   - **Name**: `Skydock` (shown to users on the consent screen).
   - **Supported account types**: **Personal Microsoft accounts only**.
   - **Redirect URI**: choose platform **Public client/native (mobile & desktop)** and enter `http://localhost`.
4. Select **Register**. On the Overview page, copy the **Application (client) ID**.
5. Open **Authentication** and set **Allow public client flows** to **Yes**. Save.
6. Open **API permissions → Add a permission → Microsoft Graph → Delegated permissions** and add:
   - `Files.ReadWrite`
   - `User.Read`
   - `offline_access`

There is no client secret for OneDrive.

### Google Drive

1. Open <https://console.cloud.google.com> and sign in.
2. Create a project: project selector at the top → **New project** → name it `Skydock`.
3. Enable the API: **APIs & Services → Library**, search for **Google Drive API**, select **Enable**.
4. Set up the consent screen: **APIs & Services → OAuth consent screen**.
   - **User type / Audience**: **External**.
   - Fill in the app name and your email address where asked.
   - Under **Test users**, add the Google account you will sign in with.
5. Create the client: **APIs & Services → Credentials → Create credentials → OAuth client ID**.
   - **Application type**: **Desktop app**.
6. Copy the **Client ID** and the **Client secret**.

Things to expect with Google:

- While the consent screen is in **Testing** status, only the test users you listed can sign in, they see an "unverified app" warning, and Google ends the session after 7 days, so you sign in again weekly.
- Removing those limits means publishing the app, and because Skydock asks for full Drive access, Google then requires its verification process. That matters for a public release, not for personal use.
- Google's "client secret" for a desktop app is not confidential; it is expected to ship inside the application.

### Give the IDs to Skydock

Copy `.env.example` to `.env` in the project folder and fill in the three values:

```sh
cp .env.example .env
```

```sh
SKYDOCK_ONEDRIVE_CLIENT_ID=...
SKYDOCK_GDRIVE_CLIENT_ID=...
SKYDOCK_GDRIVE_CLIENT_SECRET=...
```

That is all. The build reads `.env` (or the same variables from the environment) and packs the values into the binaries, so anything you build or package afterwards just shows a **Sign in** button. `.env` is git-ignored, so your registrations stay out of the repository.

The values are not secret once shipped: anyone with the binary can read them out, which is normal for a desktop app and is why providers do not treat them as confidential.

To override what a build carries without rebuilding, set the same variables in the environment, put a `.env` in the directory you start Skydock from, or use `~/.config/skydock/config.toml`:

```toml
[onedrive]
client_id = "..."

[gdrive]
client_id = "..."
client_secret = "..."
```

A build made with no values at all shows input boxes for them on each provider's card instead.

## Use

### Window

```sh
cargo run -p skydock-gui        # or `skydock-gui` once installed
```

- **Sign in** opens your browser. After you approve, Skydock reads your file list and the drive appears in its folder.
- **Check for changes** asks the drive what changed since the last check. It also happens automatically; no file content is downloaded by it.
- **Open folder** opens that provider's local folder.
- **Settings** holds the sync folder, how often to check for changes, and **Free up space**.
- Closing the window keeps Skydock in the tray; use **Quit** in the tray menu to exit.

### Command line

```sh
skydock providers          # each provider's sign-in state, storage and folder
skydock login onedrive     # or: gdrive
skydock pull onedrive      # first run reads everything, later runs fetch changes
skydock mount onedrive     # show the drive in its folder until Ctrl+C (the window does this itself)
skydock ls onedrive /Documents            # browse the fetched file list
skydock get onedrive /Documents/a.pdf     # download into the OneDrive folder, hash-checked
skydock get onedrive /Documents/a.pdf --to ~/a.pdf
skydock logout onedrive
```

From a source checkout, prefix with `cargo run -p skydock-cli --`.

### Where Skydock keeps things

| What | Where |
|---|---|
| Settings | `~/.config/skydock/config.toml` |
| File list database (names and sizes, no content) | `~/.local/share/skydock/state.sqlite` |
| Content of files you have opened | `~/.cache/skydock/` |
| Sign-in sessions | Desktop keyring |

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo fmt
```

In VS Code, `Ctrl+Shift+B` runs the window; other tasks are under **Tasks: Run Task**. `F5` debugging needs the CodeLLDB extension.

### Project layout

| Crate | Role |
|---|---|
| `crates/core` | The `Provider` trait, provider-neutral item and change types, shared sign-in, keyring storage and HTTP |
| `crates/onedrive`, `crates/gdrive` | One crate per provider |
| `crates/state` | SQLite database of remote file metadata |
| `crates/vfs` | The on-demand filesystem (FUSE) and its content cache |
| `crates/service` | Settings and the operations every front end calls |
| `crates/cli`, `crates/gui` | Command-line tool and window |

### Adding a provider

1. Add a variant to `ProviderKind` in `crates/core`.
2. Create a crate that implements the `Provider` trait. Sign-in, token storage and throttled HTTP are shared; a provider supplies its endpoints and maps its items to `RemoteItem`.
3. Add its settings and one arm in `Service::provider` in `crates/service`.
4. Add its icon name to `provider_icon` in `crates/gui`.

## Packaging

```sh
scripts/package.sh            # .deb and AppImage into out/
scripts/package.sh deb
scripts/package.sh appimage
```

The AppImage step uses `appimagetool` from your `PATH`, or downloads the official build into `out/tools/` on first use. To run the command-line tool from the AppImage: `Skydock-*.AppImage cli providers`.

Packages target Ubuntu 24.04 and newer (glibc 2.39). The script checks this and stops if a binary would need something newer.

## Licence

MIT. See [LICENSE](LICENSE).

The window is built with [Slint](https://slint.dev), used under the Slint Royalty-free Desktop, Mobile, and Web Applications License. Provider icons come from your installed icon theme; Skydock ships no Microsoft or Google artwork.
