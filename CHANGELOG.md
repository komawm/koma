# Changelog

All notable changes to Koma are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Koma uses `0.0.x`
incubation versions; `v0.1.0` will be the first independently accepted product
release. A `0.0.x` tag is a development prerelease and does not claim a working
desktop, hardware support, or security guarantees.

## [Unreleased]

### Added

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
