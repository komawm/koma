# Changelog

All notable changes to Koma are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Koma uses `0.0.x`
incubation versions; `v0.1.0` will be the first independently accepted product
release. A `0.0.x` tag is a development prerelease and does not claim a working
desktop, hardware support, or security guarantees.

## [Unreleased]

### Added

- `komactl doctor`: a non-destructive configuration diagnostic that reuses the
  jailed Shadow-VM evaluation of `komactl validate` and adds template-generation
  freshness, missing `require()` targets under `lua/`, configuration table keys
  the schema silently ignores, duplicate binds, binds that shadow a built-in
  chord, `koma.monitor` / `preferred_output` names the kernel does not report as
  connected, `XDG_RUNTIME_DIR` and `koma-admin.sock` reachability, and live
  generation / lock state. Rows are grouped by area with `ok` / `warn` / `error`
  / `skip` severities and a count summary, `--json` emits the same report for
  scripting, and `--strict` turns warnings into exit code `1`. Without
  `--strict`, a configuration error is reported but never changes the exit code,
  because the compositor keeps running on its last valid configuration.
- `komactl doctor` gained `bind.spawn-target` and `bind.spawn-target-deep`: every
  effective spawn bind (built-in catalogue, config overrides and `koma.unbind`,
  merged through `merge_binds`) resolves its program the way `execve` would — an
  absolute path must exist and be executable, a bare name is searched on `PATH` —
  and reports `warn` naming the chord and the target when the program is missing.
  `sh -c` / `fish -c` command strings are followed one level to their first
  program word. Nothing is executed: `$VAR`, globs, relative paths, shell
  keywords, built-ins, wrappers and pipelines are `skip`, and a fish function or
  alias is never resolved by running a shell. This surfaces dead binds such as
  `Mod + R` → `/usr/bin/hyprpicker` on a machine without `hyprpicker`.
- `komactl keys`: lists every effective keybind by merging the built-in
  catalogue with the user configuration (`inherit_default_binds`, `koma.unbind`,
  chord overrides), rendering chords as `Mod + Shift + 2`, marking user
  overrides while keeping the catalogue description, and listing pointer
  drag/scroll binds and touchpad gestures in their own sections. Supports
  `--json` and `--grep <pattern>`, works offline, and merges the live
  configuration path, generation, and lock state when a socket is reachable.

### Changed

- `ext-workspace-v1`: an empty workspace now reports `hidden`, so a status bar
  with `ignore-hidden: true` (waybar `ext/workspaces`) lists only the
  workspaces that hold a mapped, non-minimized window instead of every
  configured workspace. The protocol has no "occupied" state, so a
  compositor-side rule is the only way to express it. A workspace keeps its
  click target while it is occupied, and `koma.workspace({ persistent = true })`
  keeps one empty workspace visible for owners who want the full strip back.
  Workspace state is re-published when a window is mapped, unmapped or
  destroyed, moved between workspaces or minimized/restored, and on workspace
  switch and configuration reload, so the strip cannot go stale.

## [0.0.3] - 2026-11-18

### Added

- `win.move_to_workspace` (alias `win.move`) on `koma-admin.sock`: moves the
  focused workspace's active window to an existing workspace, mirroring
  `ActionDescriptor::WindowMoveToWorkspace`, with `follow` deciding whether the
  destination is focused (`movetoworkspace` vs `movetoworkspacesilent`). Rejects
  unknown workspaces and empty sources with `ErrNotFound`, applies the session
  lock gate, and is exposed as `komactl win move-to-workspace <ws_id|name>
[--follow]`.
- `ext-workspace-v1` (staging) server support: one workspace group owning every
  output, one workspace handle per Koma workspace with its id, name, 1D
  coordinates and `active` / `hidden` state, plus `output_enter` /
  `workspace_enter` bookkeeping and `done` batches. `active` marks the displayed
  workspace; `hidden` is reserved for headless/agent workspaces that have no
  on-screen representation, so ordinary inactive workspaces keep their click
  targets under `ignore-hidden: true`. `workspace.activate` is queued per
  manager and applied on `ext_workspace_manager_v1.commit` (one atomic batch,
  last request wins), routes through `KomaState::focus_workspace`, and is
  ignored while the session is locked. Group output membership retains the exact
  `wl_output` resources, so `output_leave` survives a hot-unplug and two
  bindings of one output are tracked separately. Documented subset: `urgent`,
  `deactivate`, `assign`, `remove` and `create_workspace` are not advertised.
- `focused` in each `scene.inventory` workspace entry, true for exactly one
  workspace (`active_ws_id`), replacing the runtime-file bridge for status bars.

- `Mod + M` toggles **maximize**: the focused window fills the work area minus
  gaps and exclusive zones, so the bar stays visible. `Mod + F` is true
  fullscreen: it covers the whole monitor including gaps and suppresses
  `Overlay`/`Top` layer surfaces for that output while staying mapped, so the
  bar returns the moment fullscreen ends. The two are mutually exclusive and
  both derive from one `WindowPresentation` state, so geometry, xdg states,
  foreign-toplevel and hit-testing cannot disagree. `Mod + Shift + M` keeps
  `ScrollingMaximizeColumn`, and the new Lua action is
  `koma.act.window.maximize_toggle()`.
- Dragging a window's frame edge or corner resizes it with no modifier, with
  W/E/N/S/NW/NE/SW/SE cursor shapes and the previous cursor restored on exit
  (including a client-provided one). `Mod + right-drag` still works. Deck
  refuses a tiled edge drag and says why instead of doing nothing silently;
  `LayoutMode::supports_frame_edge_resize` is the single policy table.
- `koma.input({ pointer = { follow_mouse = ... } })` is implemented with
  Hyprland's `input:follow_mouse` semantics: `off` (0, click-to-focus only),
  `normal` (1, default: focus follows and raises), and `focus_only` (2: focus
  follows without changing the workspace's active window or stacking). Unknown
  values fall back to `normal`.
- `move_to_workspace` follows the moved window to the destination workspace;
  `koma.act.window.move_to_workspace(ws, { follow = false })` keeps the source
  workspace focused (Hyprland `movetoworkspacesilent`).

- The compositor can no longer deadlock on its own seat handles. Smithay holds
  a non-reentrant mutex while it calls back into the compositor, so re-acquiring
  the same handle from inside a callback parked the only event-loop thread in
  `futex_wait` forever — no frames, no input, and no TTY, because the compositor
  still holds DRM master. A thread-local guard now turns any such re-entry into
  an `error!` naming both call sites and a soft failure instead of a hang.
  Dropping a tiled window reached exactly that chain
  (`PointerHandle::set_grab/unset_grab` → `on_ungrab` → focus change →
  `pointer.current_focus()`), the same class as the session-lock freeze.
- A stall watchdog dumps evidence from inside the compositor when the event loop
  stops ticking for `KOMA_STALL_WATCHDOG_SECS` (default 5; `0` disables): every
  thread's `comm`/`state`/`wchan`/`schedstat` plus the verbatim
  `/proc/self/task/<tid>/syscall` line with its futex address, and a
  classification (`self-reentrant-mutex`, `cross-thread-futex-cycle`,
  `kernel-blocked`, …) under `$XDG_STATE_HOME/koma/stalls/<timestamp>/`. Bounded,
  allocation-free before the files exist, and silent while healthy. Linux only;
  other targets run an inert stub.

### Fixed

- `workspace.focused` now fires for every activation path, including special
  (`special:<name>`) workspaces, output switches, output-less switches and
  `WorkspaceFocusNext` / `Prev` (cycling is a real activation and emits once),
  and never fires for a no-op re-focus or for re-opening the already focused
  special workspace. Dismissing a `special:` overlay now emits the activation of
  the underlying workspace.
- `WorkspaceFocusNext` / `Prev` route through the single workspace-switch path,
  so `ext-workspace-v1` state and `workspace.focused` stay in sync with the
  keybind instead of leaving the bar on the previous workspace.
- Workspace lookups resolve exact workspace ids before names in `ws.focus`,
  `win.move_to_workspace` and the keybind/layout path, so a workspace merely
  named `ws_01` cannot shadow workspace id `ws_01`.

- Tiled clients are no longer configured below the `xdg_toplevel` minimum size
  they advertise. Chromium and Firefox refused to shrink past their minimum and
  kept committing a cropped, un-reflowed buffer, which the compositor masked to
  the too-small tile; splitting such a window to half width showed only its left
  half. Per-window `SizeHints` now clamp every tiled geometry path, including
  fullscreen and maximize rects.
- A nested `koma --winit` presents upright. The EGL window surface handed the
  parent compositor a vertically mirrored buffer while the compose step is
  target-independent, so the nested desktop appeared upside down; the nested
  damage tracker now composes with `Flipped180` while the output transform stays
  `Normal`, and input needs no remap because the two mirrors cancel.
- `ext-session-lock-v1` acquire no longer freezes the compositor. `set_focus`
  dispatches `focus_changed` while holding Smithay's non-reentrant keyboard
  mutex, and the locked-mode IME barrier re-entered it through
  `current_focus()`, deadlocking the single event loop: no frames, no input and
  an ignored SIGTERM.
- Spawned children are reaped, so `spawn` no longer accumulates zombies that
  break `pkill X || X`.
- Client-side-decorated windows are no longer composited down and right by the
  height of their window geometry.
- Held keys repeat at 600 ms / 25 Hz, matching the Hyprland defaults a migrated
  desktop expects, and the applied values are logged.
- Translucent overlays no longer wash the screen: solid-color elements
  premultiply their straight-alpha colors for the premultiplied GLES blender, so
  the drag drop-zone fill, scratchpad scrim, grid hairlines and tab indicators
  render at their intended opacity instead of `1/alpha`.
- `Mod + left-drag` moving a window no longer flashes the background: the
  drop-zone fill was the same premultiplication bug, painted at full intensity.
- `komactl init [dir] [--force]` scaffolds a commented configuration tree
  (`init.lua`, `lua/input.lua`, `lua/workspaces.lua`, `lua/appearance.lua`,
  `lua/window_rules.lua`, `lua/keybinds.lua`, `lua/macros/example.lua`). The
  keymap is a full expansion of a single Rust keybind catalogue shared with the
  runtime defaults, and the tree validates with `inherit_default_binds = false`
  and no `koma.monitor` block. Existing files are refused unless `--force` is
  given. The header carries a template version marker, and `komactl config-status`
  reports when the on-disk template is older than the shipped one.
- `koma init <bash|zsh|fish|nushell> [--unset]` prints the live instance
  environment (`WAYLAND_DISPLAY`, `KOMA_INSTANCE_SIGNATURE`, `XDG_RUNTIME_DIR`,
  `DISPLAY` while XWayland is up, `XDG_SESSION_TYPE`, `XDG_CURRENT_DESKTOP`) so
  `koma init fish | source` works from an external terminal. It scans
  `$XDG_RUNTIME_DIR/koma/` read-only, fails clearly when zero or several
  instances are live, and never prints capability tokens.
- First-run auto-scaffold for a missing `$XDG_CONFIG_HOME/koma/init.lua`,
  disabled with `KOMA_NO_SCAFFOLD=1`, skipped for `--headless-check`, and
  compiled out of test builds.
- `koma.act.reload_config()` and `koma.act.quit()` Lua actions so the generated
  keymap can express `Mod + Shift + R` and `Mod + Shift + E` without a native
  code path.

### Changed

- The standalone `koma-mcp` binary is now `koma mcp [--print-tools]
[--auto-mint-admin]`; release archives, Nix builds, BSD CI and completions no
  longer install or reference a separate binary.
- `appearance.animations.windows_close` and `close_scale_to` now drive a real
  close animation. Each mapped toplevel's last committed frame is cached in GPU
  memory on `wl_surface.commit` (the only moment a client buffer is readable),
  read back once when the window leaves the layout, and composited with a
  `windows_close` shrink/fade spring while the window is already gone from the
  layout, hit-testing, focus and `scene.inventory`. `WindowAnimationTable`
  reports the close spring through `start_close`, `sample_close`,
  `is_closing` and `take_settled_closing`, and the frame loop reaps the captured
  frame only when the spring settles. With animations disabled, a
  `"none"`/`instant` close style, or no renderer, the window's state is freed
  synchronously as before.

## [0.0.2] - 2026-11-17

### Added

- Installable agent skill at `skills/koma-desktop/` with an MIT `LICENSE` inside
  the skill directory, a thin `SKILL.md` router, and `references/` for
  workspaces, input primitives, and macro Lua. It installs with
  `npx skills add komawm/koma --skill koma-desktop`.
- `scripts/check-skills.py` (wired into `just check` and CI): validates skill
  frontmatter, `name`/directory agreement, MIT licensing, metadata, and relative
  links, and rejects links that escape the skill directory.
- Shell completions for `koma` and `komactl` in bash, zsh, and fish, exposed as
  `koma completions <shell>` and `komactl completions <shell>`. `komactl`
  renders them from a declarative table kept in sync with `usage_text()` by a
  test; `koma` cross-checks its static scripts against `cli::usage()`.

### Changed

- `examples/SKILL.md` became `examples/coding-agent-cli.md`: a plain CLI-first
  walkthrough with no frontmatter, so it can no longer be picked up as a second
  `koma-desktop` skill by a `--full-depth` scan. Its stale "proposed interfaces"
  warning is gone.
- `koma_exec_macro`'s MCP description now says it returns executor status and
  epoch, not a macro result, and that a suspended macro completes
  asynchronously and is cancelled if the workspace epoch changes.
- The skill validator rejects YAML booleans/nulls and empty `license` values.

## [0.0.1] - 2026-11-16

### Added (agent cursor precision, vSeat trajectories & MCP)

- Multi-waypoint and heading-continuous agent cursor trajectories in `koma-model`
  (`PathSplineKind::{Auto, CatmullRom, Bezier}`, centripetal `alpha = 0.5`
  `CentripetalCatmullRom`, 250 Hz adaptive sub-frame resampling, and minimum-jerk
  kinematics) exposed via `vseat.path`, `vseat.stroke`, and Lua
  `MacroContext:{move_path, stroke_path, path, settle_cursor}`.
- Extended `vseat.*` compositor RPCs in `koma` (`vseat.relative_move`,
  `vseat.wheel`, `vseat.click` with `modifiers` + `label`, `vseat.hit_test`,
  `vseat.cursor_label`, `vseat.cursor_settle`) alongside Smithay `ClickGrab`
  drag-motion/button-release forwarding for reliable spline strokes.
- GLES2 SDF agent cursor indicator and floating `LabelPill` + `caret_rect`
  companion overlay (`LabelPillSnapshot` rasterization in `koma-render`), with
  screencopy/capture isolation controls (`koma.capture`).
- Explicit IPC error delivery metadata (`DeliverySemantics`,
  `DeliveryDisposition`, `RetryDisposition`) in `koma-proto` so clients can
  distinguish un-delivered, uncertain, and verified pointer/key actions.
- Lazy-connect `koma-mcp` stdio server supporting auto-detected `Content-Length`
  and NDJSON framing across all 17 Koma MCP tools (`koma_ping`,
  `koma_scene_inventory`, `koma_ws_create`, `koma_capture_frame`, `koma_click`,
  `koma_drag`, `koma_key`, `koma_inject_text`, `koma_paste_mime`,
  `koma_win_activate`, `koma_exec_macro`, `koma_path`, `koma_cursor_settle`,
  `koma_wheel`, `koma_relative_move`, `koma_hit_test`, `koma_cursor_label`).

Initial development prerelease of the imported incubation baseline.

### Added

- Nine-crate Rust 2024 workspace targeting Rust `1.100.0`: `koma-proto`,
  `koma-model`, `koma-lua`, `koma-render`, `koma-seat`, the `koma` compositor
  binary, `komactl`, `koma-mcp` and `koma-overview`.
- Lua configuration through the Phodopus sandbox with configurable defaults:
    - `Dwindle` is the default workspace layout; `koma.layout(...)`,
      `koma.workspace({...})`, `koma.dwindle({...})` and `koma.scrolling({...})`
      select and tune layouts globally or per workspace.
    - Hyprland-compatible default keybinds loaded when
      `inherit_default_binds = true`; `koma.bind(...)` overrides individual
      chords in place and `koma.unbind(chord)` removes specific defaults.
    - Typed appearance settings for borders, focus ring, shadow, dim, blur and
      animations.
- `komactl gen-types` emits LuaLS definitions for the configuration API;
  `komactl validate` checks a configuration offline.
- Compatibility & input protocols:
    - `wp_single_pixel_buffer_v1` and `wp_presentation` feedback alongside
      `wl_shm` `Abgr8888`/`Xbgr8888` pixel formats for `waybar`, `ironbar`,
      `quickshell`, and Android/`Waydroid` surfaces.
    - Full `wlr_layer_shell` `get_popup`, `xdg_popup` unconstraining and
      pointer/keyboard grabs, plus `zwlr_virtual_pointer_manager_v1` and
      `zwp_keyboard_shortcuts_inhibit_manager_v1` (with `Mod + G` / VT switch
      kept un-inhibitable) for `RustDesk` and remote desktop tools.
    - 2D damped Hooke's law semi-implicit Euler spring kinematics (`pointer.spring` /
      `pointer.curve`) for smooth macro & cursor motion, plus smart `Waydroid`
      `no_blur`, `clip_to_geometry` gating, and empty `input_region` click-through.
- Session resources (desktop entry, systemd units, portals configuration,
  greetd example) and example configuration under `examples/`.
- Shared local/CI quality gates (`just check`: descriptor validation,
  shellcheck, actionlint, rustfmt, cargo check, clippy with `-D warnings`,
  tests), GitHub Actions CI and CodeQL workflows pinned by commit SHA.
- Release tooling: `scripts/package-release.sh`, `just package-release`, and a
  tag-triggered workflow that publishes development prereleases with a
  release manifest and `SHA256SUMS`.

### Changed

- Reconciled core geometry/window identifiers, the Phodopus bridge, protocol
  exports and Tier 2/3 geometry/lease APIs so the full workspace builds and
  passes clippy and tests.
- Dropped the redundant `license-file` manifest field; crates declare
  `license = "GPL-3.0-or-later"` with the single root `LICENSE`.

### Known limitations

- No bare-metal hardware, DRM/KMS, physical input or GPU acceptance has been
  performed.
- Headless QEMU/KVM and nested `winit` compositor smoke checks have verified
  virtual seat (`vseat`) agent pointer trajectories, whereas full multi-app
  daily-driver soak testing remains pending.
- Koma is not yet a supported daily-driver desktop. Interfaces, configuration
  and IPC may change without notice on the `0.0.x` line.
- Prebuilt archives cover `x86_64-unknown-linux-gnu` and
  `aarch64-unknown-linux-gnu`; BSD tier builds remain compile-verified while
  BSD release packaging is planned.

[Unreleased]: https://github.com/komawm/koma/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/komawm/koma/releases/tag/v0.0.1
