# OneDrive client for Linux: research notes

Gathered 2026-10-06. Items marked **(unverified)** come from memory or a search snippet rather than a primary source read in full; check them before relying on them.

## 1. Inspired by

Skydock builds on ideas from projects that came before it. Each of these was read while planning, and each shaped a decision.

| Project | Approach | What Skydock takes from it |
|---|---|---|
| [abraunegg/onedrive](https://github.com/abraunegg/onedrive) | D, command-line sync daemon, GPLv3 | The reference for correct OneDrive sync: delta handling, WebSocket change notifications, shared folders, and its documentation of Graph API quirks |
| [jstaf/onedriver](https://github.com/jstaf/onedriver) | Go, FUSE, GPLv3 | Showed that a FUSE mount gives files on demand on Linux without root, and what to watch for with thumbnails and large files |
| [franzjeger/OneDriveForLinux](https://github.com/franzjeger/OneDriveForLinux) | Rust workspace, FUSE, MIT | The crate layout (API client, sync engine, filesystem, daemon, tray, CLI) and exposing sync state to Dolphin through an extended attribute |
| [kartas39/konedrive](https://github.com/kartas39/konedrive) | Rust daemon with a Qt6/Kirigami window, fanotify, GPLv3 | The fanotify pre-content approach as an alternative to FUSE, keeping tokens in the desktop keyring, and a window that talks to the daemon over D-Bus |
| [rclone](https://rclone.org/onedrive/) | Go, mount with a local cache | Letting users supply their own client ID, and its long experience with both providers' APIs |

Where Skydock aims to differ is in combination, not in any single idea: files on demand, more than one provider, a desktop window, and `.deb` and AppImage packages together.

## 2. Authentication (Microsoft identity platform v2)

- Endpoints: `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize` and `/token`. `tenant` is `common` (both account types), `consumers`, `organizations`, or a tenant ID.
- This is a public client: authorization code flow with PKCE (`code_challenge_method=S256`), no client secret. Redirect URI `http://localhost` for system-browser sign-in. Any loopback port is accepted for a registered `http://localhost` URI **(unverified)**.
- Device code flow is the fallback for headless use. Tenants with Conditional Access often block it, so browser sign-in must be the default.
- Scopes: `offline_access` (required to get a refresh token), `Files.ReadWrite` for the user's own drive, `Files.ReadWrite.All` for items shared with the user, `User.Read`; `Sites.Read.All` only if SharePoint libraries are in scope.
- Access tokens last about an hour (`expires_in: 3599`). Refresh tokens have no fixed lifetime and can be revoked at any time; every refresh may return a new refresh token, which must replace the stored one.
- Do not parse access tokens. Personal-account tokens are opaque or encrypted.
- An Entra app registration (client ID) is needed, with "personal + work/school accounts" audience and public client flows enabled. Open question: ship one client ID, and allow users to override it (konedrive and abraunegg both allow override).
- Token storage: Secret Service D-Bus API (KWallet provides it on Plasma 6), never a plain file.

## 3. Delta sync

Source: [driveItem: delta](https://learn.microsoft.com/en-us/graph/api/driveitem-delta?view=graph-rest-1.0), [scan guidance](https://learn.microsoft.com/en-us/onedrive/developer/rest-api/concepts/scan-guidance?view=odsp-graph-online).

- `GET /me/drive/root/delta` (or `/drives/{id}/root/delta`). Follow `@odata.nextLink` until a page returns `@odata.deltaLink`; persist the deltaLink only after the whole set has been applied.
- Delta is the only enumeration guaranteed complete; paging `children` can miss items under concurrent writes.
- `?token=latest` returns just a deltaLink with no items (use when existing state is not needed).
- Timestamp tokens work on Business/SharePoint only.
- The feed gives the latest state per item, not a change log. An item may appear more than once; the last occurrence wins.
- `parentReference.path` is absent. Renaming a folder does not emit its descendants. **Track everything by item ID and derive paths locally.**
- Deleted items carry the `deleted` facet. Delete a local folder only if it is empty after applying the whole batch.
- Missing properties: Business omits `cTag` on create/modify and `cTag` + `name` on delete; Personal omits `cTag` + `size` on delete.
- `410 Gone` means the token is dead. The `Location` header holds a fresh nextLink for full re-enumeration, and the error code says how to reconcile:
  - `resyncChangesApplyDifferences`: server wins for items known to be in sync; upload local-only changes.
  - `resyncChangesUploadDifferences`: upload anything the server did not return or that differs, keeping both copies when unsure.
- `deltaExcludeParent` header drops unchanged parent folders from responses.
- Shared folders added to a personal drive appear as items with a `remoteItem` facet pointing at another drive, which needs its own delta. Since the personal-account backend migration, shortcuts may need `Prefer: Include-Feature=AddToOneDrive` **(unverified)**, and personal `driveId` values have been returned with inconsistent case and 15 instead of 16 characters ([abraunegg #3072](https://github.com/abraunegg/onedrive/issues/3072)). Normalise drive IDs (lowercase, left-pad to 16) before using them as keys.

### Change notifications

- Webhooks need a public HTTPS endpoint, so they are unusable on a desktop.
- `GET /me/drive/root/subscriptions/socketIo` returns a `notificationUrl` for a socket.io endpoint; delegated permissions only, works for personal and work accounts. Must use the `websocket` transport (polling is unsupported). A notification carries no payload worth trusting; it is only a signal to run delta. ([docs](https://learn.microsoft.com/en-us/graph/api/subscriptions-socketio?view=graph-rest-1.0))
- The URL expires and must be re-requested periodically **(unverified: exact lifetime)**. Keep a slow polling fallback (konedrive uses 5 minutes).

## 4. Transfers

Download ([docs](https://learn.microsoft.com/en-us/graph/api/driveitem-get-content?view=graph-rest-1.0)):

- `GET /drives/{d}/items/{id}/content` answers `302` to a pre-authenticated URL (same as `@microsoft.graph.downloadUrl`). It may expire within minutes, so fetch it right before use and never store it.
- `Range` requests go to the download URL, not to `/content`, and without an `Authorization` header. A `200` instead of `206` means the range was ignored and the full body is coming. This is what makes partial hydration and resumable downloads possible.

Upload ([docs](https://learn.microsoft.com/en-us/graph/api/driveitem-createuploadsession?view=graph-rest-1.0)):

- Simple `PUT .../content` for small files (documented limit 250 MB **(unverified)**); upload sessions recommended above 10 MiB. Zero-byte files must use simple PUT **(unverified)**.
- Session: `POST .../items/{parentId}:/{name}:/createUploadSession` (new) or `.../items/{id}/createUploadSession` (replace). Body may set `@microsoft.graph.conflictBehavior` (`fail` default, `replace`, `rename`) and `fileSystemInfo` to preserve mtime.
- Fragments are sequential, each a multiple of 320 KiB (327,680 bytes), under 60 MiB, 5 to 10 MiB recommended. Non-multiples can fail at commit.
- No `Authorization` header on PUTs to `uploadUrl` (can cause 401).
- Resume with `GET uploadUrl` and read `nextExpectedRanges`. `416` means the server already has that fragment. `404` means the session is gone: restart.
- Use `If-Match: {eTag}` when replacing so a concurrent remote edit yields `412` rather than a silent overwrite. `409 nameAlreadyExists` can arrive on the final fragment.
- Max file size 250 GB.

Integrity:

- `quickXorHash` is the only hash guaranteed on both Personal and Business. `sha1Hash` was deprecated on Personal in 2023; `sha256Hash` should not be used. QuickXor must be implemented locally (small, public algorithm).
- `cTag` changes when content changes, `eTag` when anything changes.
- SharePoint/Business may rewrite Office files after upload, so size and hash can differ from what was sent **(unverified, widely reported)**. Do not treat that as a conflict or re-upload loop.

## 5. Throttling

Source: [SharePoint throttling](https://learn.microsoft.com/en-us/sharepoint/dev/general-development/how-to-avoid-getting-throttled-or-blocked-in-sharepoint-online).

- `429` and `503` both carry `Retry-After`. Pause **all** requests for that duration, not only the failing one. Throttled requests still count against the quota.
- SharePoint does not send IETF `RateLimit` headers; do not depend on them.
- Per-user default: 3,000 requests per 5 minutes, 50 GB ingress and 100 GB egress per hour.
- Cost: delta with a token is 1 resource unit, without a token 2; create/update/upload 2; anything touching permissions 5.
- Decorate traffic: `User-Agent: NONISV|<Company>|<App>/<Version>` (or `ISV|...`). Decorated traffic is prioritised.

## 6. Name and path rules

Source: [restrictions and limitations](https://support.microsoft.com/en-us/office/restrictions-and-limitations-in-onedrive-and-sharepoint-64883a5d-228e-48f5-b3d2-eb39e07630fa).

- Forbidden characters: `" * : < > ? / \ |`. No leading or trailing spaces.
- Forbidden names: `.lock`, `CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`, `LPT0`-`LPT9`, `desktop.ini`, anything containing `_vti_`, anything starting with `~$`; `forms` at a library root.
- OneDrive is case-insensitive and case-preserving; ext4 is case-sensitive. `a.txt` and `A.txt` in one local folder cannot both upload.
- No symlinks, no POSIX permissions or ownership.
- Legal on Linux but illegal on OneDrive names need an explicit policy (skip and report, not silently rename).

## 7. Files On-Demand on Linux

There is no Linux equivalent of the Windows Cloud Files API. Two workable mechanisms:

**FUSE** (onedriver, OneDriveForLinux)

- Unprivileged via `fusermount3`; works on any kernel.
- The daemon serves `getattr`/`readdir` from the metadata DB so files show their real size without being downloaded; `open`/`read` hydrate into a cache.
- All I/O crosses userspace. FUSE passthrough (kernel 6.9+) hands hydrated files back to the kernel but currently needs `CAP_SYS_ADMIN` **(unverified)**.
- If the daemon dies the mount is dead ("Transport endpoint is not connected"); needs clean unmount handling and a systemd user unit.
- Rust: [`fuser`](https://crates.io/crates/fuser) 0.18 (2026-07), libfuse3 preferred, multi-threaded sessions, experimental async API. Alternatives: `fuse3`, `rfuse3` (async-first).

**fanotify pre-content events** (konedrive)

- Real sparse files on the real filesystem; `FAN_PRE_ACCESS` with `FAN_CLASS_PRE_CONTENT` suspends the reader, reports the byte range, and the handler writes the content before allowing the access. Can deny with `FAN_DENY_ERRNO` (`EIO`, `EBUSY`, `ENOSPC`, ...).
- Native I/O speed once hydrated, and files stay readable if the daemon is down (if already hydrated).
- Needs a recent kernel (konedrive states 6.14 for full behaviour) and a privileged helper, since permission-class fanotify groups require `CAP_SYS_ADMIN` **(unverified against man pages; konedrive ships a root helper for this reason)**.
- This machine runs kernel 7.0, so it is available here, but a root helper weakens the "trusted, simple deb" goal and rules out a self-contained AppImage.

Common problems either way:

- File managers generate thumbnails by reading files, which downloads everything. Mitigations: serve thumbnails from Graph (`/thumbnails`) through a KIO thumbnailer plugin, and refuse or defer hydration for known indexer/thumbnailer processes (Baloo must be excluded too).
- States to model per item: online-only, hydrating, local, pinned ("always keep"), plus dirty/uploading/conflict/error.
- Eviction ("free up space") must never drop a file with un-uploaded changes.

## 8. KDE Plasma integration (Kubuntu first)

- **Overlay icons**: `KOverlayIconPlugin` (KF6 KIO), installed with `kcoreaddons_add_plugin(... INSTALL_NAMESPACE "kf6/overlayicon")`. `getOverlays(QUrl)` runs on Dolphin's main thread and must not block: answer from a cache and emit `overlaysChanged` later. Both prior Rust projects expose state as an xattr (`user.<app>.state`) that the plugin reads, which avoids a D-Bus round trip per file.
- **Context menu**: `KAbstractFileItemActionPlugin` (namespace `kf6/fileitemaction`) for "Always keep on this device", "Free up space", "Copy link". A plain `.desktop` ServiceMenu calling the CLI is the zero-C++ alternative.
- **Tray**: StatusNotifierItem over D-Bus (Rust: `ksni`). Works on Plasma Wayland; no XEmbed.
- **Places sidebar**: add an entry to `~/.local/share/user-places.xbel`.
- **Notifications**: `org.freedesktop.Notifications`.
- **Autostart/lifecycle**: systemd user service.
- These plugins are C++/Qt shared objects loaded into Dolphin from the system plugin directory. They cannot live inside an AppImage, so the deb is the primary package and an AppImage would be a reduced-integration build.
- Known hazard: overlay plugins broke for Nextcloud and Insync in the Plasma 5 to 6 transition; they must be built against KF6.
- GNOME later: Nautilus extension (libnautilus-extension / Python) and `libcloudproviders`.

## 9. Language assessment

| | Rust | C# (.NET 10 / Uno) | Python |
|---|---|---|---|
| FUSE | `fuser`, mature | Thin, poorly maintained bindings | `pyfuse3`, workable but slow under the GIL |
| Footprint | Single small native binary | Native AOT possible; Uno/Skia UI is heavy | Interpreter + venv in the package |
| D-Bus / tray | `zbus`, `ksni` | `Tmds.DBus` | `dbus-next` |
| Graph | Hand-rolled REST over `reqwest` (small surface) | Official Graph SDK + MSAL | `msal` + REST |
| Packaging | `cargo-deb`, AppImage straightforward | Self-contained publish is large | Hardest to make a clean deb/AppImage |
| Prior art | Two recent on-demand clients | None found on Linux | Earlier sync clients |

Recommendation: Rust for daemon, VFS and CLI. The needed Graph surface is about a dozen endpoints, so the lack of an official SDK costs little, while FUSE quality and package size matter a lot. The Dolphin plugins are C++ regardless of the core language.

## 10. Local environment (2026-10-06)

- Kubuntu 26.04.1, Plasma 6.6.6 on Wayland, Dolphin 25.12.3, kernel 7.0.0-38.
- Installed: `fusermount3` 3.18.2, .NET SDK 10.0.112, Python 3.14.4.
- Not installed: Rust toolchain (apt offers 1.93.1; rustup is the alternative), `libfuse3-dev`, `libkf6kio-dev` (6.24.0), `extra-cmake-modules`.
- No other OneDrive client or rclone is installed, so nothing competes for the account or mount point.
