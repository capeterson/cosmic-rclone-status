# cosmic-rclone-status

A [COSMIC](https://github.com/pop-os/cosmic-epoch) panel applet that shows the status of an
[rclone](https://rclone.org) mount: synced, uploading, downloading, both, error, or mount down.
Click the panel icon for details, per-file transfer progress, the 10 most recently synced files,
and Open folder / Restart / Stop buttons. "Show all" opens the full history (up to 100 files).
Clicking a file opens its details window (see below). Files get the same type icons as COSMIC Files.

Status is read from rclone's remote-control API (`--rc`: `core/stats`, `vfs/stats` and
`core/transferred`) plus `systemctl --user` and `/proc/mounts`.

## Requirements

- COSMIC desktop, Rust toolchain, and libcosmic's system dev libraries
  (`libxkbcommon-dev libfontconfig-dev libfreetype-dev libexpat1-dev libudev-dev libinput-dev libegl-dev libgbm-dev wayland-protocols`)
- rclone mounted by a systemd **user** service with the rc API enabled and authenticated, e.g.
  `rclone mount remote: ~/Drive --rc --rc-addr 127.0.0.1:5572` with `RCLONE_RC_USER` / `RCLONE_RC_PASS`
  provided through an `EnvironmentFile`.

## Configuration

Defaults (stored in cosmic-config under `com.ca-peterson.CosmicRcloneStatus`):

| Key | Default |
|---|---|
| `rc_addr` | `127.0.0.1:5572` |
| `env_file` | `~/.config/rclone/gdrive-rc.env` |
| `service` | `rclone-gdrive.service` |
| `mount_path` | `~/GoogleDrive` |
| `cache_dir` | `~/.cache/rclone` (rclone's `--cache-dir`; read for per-file cache status) |
| `context_menu` | `true` (add the "Rclone…" entry to COSMIC Files' right-click menu) |
| `offline_enabled` | `false` (offline folders; switch on in Settings) |
| `pins` | `[]` (folders kept offline, relative to the mount) |
| `sync_interval_secs` | `300` (how often offline folders sync when nothing changes locally) |

The settings window (`--settings`, or the gear in the popup) covers offline folders and the
right-click menu entry.

## Offline folders

Like Google Drive's "Available offline": a pinned folder is fully downloaded, kept in sync both
ways, and works with no connection, right where it is in the Drive folder. Turn it on in
Settings, then right-click a folder in COSMIC Files → **Rclone…** → **Keep offline**.

How it works:

- The folder's local copy lives in `~/.local/share/cosmic-rclone-status/offline/<id>` and is
  synced with `rclone bisync`, run *inside the mount's own rclone process* through its rc API
  (`sync/bisync`), so there is no second rclone config or login. A background user service
  (`cosmic-rclone-status-offline.service`) runs each sync: every `sync_interval_secs`, and about
  10 seconds after a local change.
- The local copy is mounted on top of the folder's place in the Drive folder
  (`cosmic-rclone-status-pin@<id>.service`), so it shows up in place. Both services are bound to
  the mount's service and start and stop with it; a folder that has been downloaded is mounted
  even when offline.
- Conflicts (the same file changed on both sides between syncs) are resolved as *newer wins*:
  the older edit is uploaded first, so it stays in the file's version history in Google Drive
  ("Manage versions"; Drive keeps old versions of uploaded files for about 30 days or 100
  versions unless one is marked "Keep forever"). You get a notification.
- Native Google Docs, Sheets and Slides are left out of the offline copy (rclone can only
  download exports of them, which can't be synced back); they still open from the main mount.
- If bisync stops for safety (too many deletions, or the folder vanished in the cloud), the
  folder is marked **Needs attention**; **Repair** in Settings merges both sides without deleting
  anything.
- Unpinning uploads the last changes first (it waits if you're offline), then unmounts and deletes
  the local copy.

Setup, done from Settings: mounting inside the Drive mount needs the Drive mount to use
`--allow-other`, which ordinary users can only request once `user_allow_other` is set in
`/etc/fuse.conf` (one line, added through the system password prompt). The app then adds a
drop-in for the mount's service (`<service>.d/50-cosmic-rclone-status.conf`) that sets
`--allow-other` together with `--default-permissions` and a `077` umask, so other users still
can't open your files, and restarts the mount once. Turning offline folders off removes the
services and the drop-in; the `/etc/fuse.conf` line is left in place.

Limits: folders only (not single files), and not nested inside each other; moving a file into or
out of an offline folder is a copy plus a delete (it crosses mounts); very long folder paths are
refused because rclone 1.60's bisync can't name its working files.

Command line: `--pin PATH`, `--unpin PATH`, `--pins`, `--offline-repair FOLDER`, and
`--offline-remove` (used by `just uninstall-user`; refuses while folders are pinned).

## Rclone details window

Right-click a file or folder in COSMIC Files and choose **Rclone…** (the applet adds this entry
to COSMIC Files' `context_actions` config when it starts; COSMIC Files cannot nest custom
actions in a submenu, so it is one entry that opens this window). Clicking an item in the
applet's recent-files list opens the same window. It shows:

- **Sync status**: for a file, whether it is in sync, online only, uploading, queued, downloading,
  partly downloaded, or differs from Google Drive, with Drive's modified time, local cache state,
  size and version; for a folder, counts of files uploading, queued and downloading and how much
  is cached. Local state comes from rclone's rc API and its VFS cache metadata on disk.
- **Sharing**: whether the item is shared and a table of who has access (name, role, type).
- **Open** (a file expands to *Folder* / *File*; a folder opens directly) and **Manage Sharing**,
  which opens the item in Google Drive with the sharing dialog requested.

Google Drive data comes from the Drive API using the OAuth token rclone already holds: the
applet asks rclone's rc API for the current token, so it never reads `rclone.conf` or stores
credentials. Only Google Drive remotes are supported for now. `Manage Sharing` relies on the
`sharingaction=manageaccess` URL parameter, which Google does not document; if it stops working
the item still opens in Drive, just without the dialog.

Command-line helpers: `--menu PATH` opens the window, `--share-url PATH` prints the Manage
Sharing link, and `--menu-install` / `--menu-remove` add or remove the COSMIC Files entry.

## Build and install

```
cargo install just
just install-user      # builds and installs under ~/.local
```

Then add "rclone Status" in COSMIC Settings → Desktop → Panel → Configure panel applets.
`cosmic-rclone-status --status` prints one status snapshot without the panel.

## Notes

rclone does not report transfer direction, so a transfer counts as a download when there are more
active transfers than VFS uploads in progress. This is a heuristic.

The recent-files history comes from rclone's in-memory list of completed transfers, so it
starts empty and resets when rclone restarts. Repeats of the same file are shown as one row
(`×N`) at its latest time, and new entries appear after a 2 second delay so the list doesn't
jump on every poll. Newer rclone versions report each transfer's source and destination; when
they do, rows get an upload or download arrow. Older versions (such as 1.60) show no arrow.
File icons are chosen from the file name alone (shared-mime-info plus your icon theme); files
are never read, so nothing is downloaded to draw the list.

## License

GPL-3.0-only
