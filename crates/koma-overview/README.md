# koma-overview

Layer-shell daemon, hover state machine, overview/card geometry and control IPC. Default `wayland-layer-shell` integration needs a running compatible display backend and separate visual tests.

## Daemon lifecycle

`koma-overview --daemon` owns one `wlr-layer-shell` connection and one control socket at
`$XDG_RUNTIME_DIR/koma/<instance_signature>/koma-overview.ctl.sock`, next to
`koma-overview.pid`. Every exit path reports a reason on stderr and through `tracing` (startup
failure, requested `quit`, lost Wayland connection, event-loop error, or panic), so a vanished
daemon leaves a journal line instead of silence. A panic hook routes panics to the same stream.

A layer surface that the compositor closes or never configures is recreated up to
`MAX_SURFACE_RECREATES` times while the control socket keeps answering; the daemon no longer exits
because one surface failed.

`koma-overview toggle` self-heals: on `ECONNREFUSED`/`ENOENT` it starts one replacement daemon
(single-flight through `koma-overview.spawn.lock`) and retries the toggle once. Attempts are bounded
by the shared `koma-overview.spawn-state` budget (`MAX_RESPAWNS` inside `RESPAWN_WINDOW`, exponential
backoff), and the give-up diagnostic is logged exactly once. The compositor supervises the daemon it
starts the same way, through `crate::child_reaper::spawn_detached`, and `komactl doctor` reports
`environment.overview-daemon` as `ok` (socket answers), `warn` (socket file without a listener) or
`skip` (no daemon for this instance).

This is one crate within [Koma](https://github.com/komawm/koma), not a separate Git repository. See the [workspace manifest](https://github.com/komawm/koma/blob/main/Cargo.toml), [architecture](https://github.com/komawm/docs/blob/main/docs/ARCHITECTURE.md), [agent guidance](https://github.com/komawm/koma/blob/main/AGENTS.md), and [testing plan](https://github.com/komawm/docs/blob/main/docs/development/testing.md).
