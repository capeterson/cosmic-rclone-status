# cosmic-rclone-status

A [COSMIC](https://github.com/pop-os/cosmic-epoch) panel applet that shows the status of an
[rclone](https://rclone.org) mount: synced, uploading, downloading, both, error, or mount down.
Click the panel icon for details, per-file transfer progress, the 10 most recently synced files,
and Open folder / Restart / Stop buttons. "Show all" opens the full history (up to 100 files).
Clicking a file opens the folder that contains it. Files get the same type icons as COSMIC Files.

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
