# Session integration resources

This directory contains [session wrapper](https://github.com/komawm/koma/blob/main/resources/koma-session), [desktop entry](https://github.com/komawm/koma/blob/main/resources/koma.desktop), [systemd user service](https://github.com/komawm/koma/blob/main/resources/koma.service), [shutdown target](https://github.com/komawm/koma/blob/main/resources/koma-shutdown.target), [portal routing](https://github.com/komawm/koma/blob/main/resources/koma-portals.conf), and [greetd example](https://github.com/komawm/koma/blob/main/resources/greetd-config.toml).

The [installer](https://github.com/komawm/koma/blob/main/resources/install-session.sh) writes system integration files using configurable `PREFIX` and `SYSCONFDIR` locations. The wrapper can start/stop session services and change graphical-session environment. Inspect and stage these resources separately after build/runtime validation; do not activate them during workspace initialization or an ordinary nested test.

[jikud.service](https://github.com/komawm/jiku/blob/main/resources/jikud.service) is a proposed companion-service template. The service template now belongs to the separate Jiku repository. No implemented `jikud` binary exists, and Koma's installer does not install this template. Its filesystem restrictions are design inputs requiring dedicated validation once a daemon exists.

## Seat management and VT switching

`koma --tty` opens DRM and input devices through libseat, which talks to either `seatd` or `systemd-logind`. Start it from the virtual terminal it will own; started over SSH or from an inactive VT it cannot become DRM master.

- **seatd**: enable `seatd.service`, add the user to the `seat` group, and set `LIBSEAT_BACKEND=seatd` if logind is also running. VT switching (`Ctrl+Alt+F1..F12`) works for the active session without further configuration.
- **systemd-logind**: device access works for the active session, but VT switching calls logind's `SwitchTo`, which an unprivileged session may only use when polkit authorizes `org.freedesktop.login1.chvt`. Without a running polkit agent/daemon the switch fails with `EACCES`; Koma logs `VT switch failed` with this cause. Install and run polkit, or use seatd.

Smithay logs `Unable to become drm master, assuming unprivileged mode` whenever the seat manager, not Koma, opened the DRM device: the kernel refuses `SET_MASTER` from another process even though the fd already holds master. Koma checks the actual master state right before and logs whether master was granted, so the Smithay warning alone is not an error.

On display-only DRM devices without a render node (`bochs-drm`, `simpledrm`, `virtio-gpu` without virgl), Koma falls back to Mesa's software rasterizer (llvmpipe through GBM on the card node) and does not advertise `zwp_linux_dmabuf_v1`, so clients use shared memory. Set `KOMA_DISABLE_SOFTWARE_RENDERING=1` to refuse this fallback. If no output can be driven, `koma --tty` exits with status 1 and the per-device causes instead of running without a `wl_output`. `SIGTERM`, `SIGINT` and `SIGHUP` stop the compositor gracefully (exit status 0), releasing DRM master, the seat session and the IPC sockets.
