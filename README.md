# Koma

Koma is a Wayland tiling compositor designed for keyboard-driven desktops and concurrent GUI automation. Its Rust workspace separates window state, IPC, rendering, seat handling, and a sandboxed Lua policy layer built on [Phodopus](https://github.com/Xuepoo/phodopus).

The repository already contains substantial implementation; canonical architecture specifications live in [koma-docs](https://github.com/komawm/docs). It is an early development baseline: source presence and specification targets do not establish build, runtime, security, latency, or platform acceptance. Start with the [current-state inventory](https://github.com/komawm/docs/blob/main/docs/development/current-state.md) and [development guide](https://github.com/komawm/docs/blob/main/docs/development/README.md).

[Jiku](https://github.com/komawm/jiku) is the separate GUI orchestration companion. Its repository defines pure-data contract types, the Three-Driver model routing architecture (`DecisionsDriver`, `TurnModelDriver`, and `LiveSessionDriver`), `DecisionCompiler` primitives, and configurable defaults with zero-error `EmulatedStructuredTextModel` fallback, while remaining a library-only scaffold prior to `v0.1.0`.

## Configurable defaults and Lua customization

Koma avoids hardcoded behavior in favor of sensible defaults that can be overridden in `~/.config/koma/init.lua`:

- **Default layout (`Dwindle`)**: Headed workspaces default to `LayoutMode::Dwindle` and can be changed globally via `koma.layout("dwindle" | "scrolling" | "deck" | "bsp")` or per-workspace via `koma.workspace({ layout = "...", gaps = { inner = 8, outer = 12 } })`, with fine-grained tuning in `koma.dwindle({...})` and `koma.scrolling({...})`.
- **Hyprland-compatible default keybinds with selective override**: When `inherit_default_binds = true` (default in `koma.input({...})`), Koma loads the built-in Hyprland-style default keymap (`Mod + Return`, `Mod + Q`, `Mod + M`, `Mod + V`, `Mod + F`, `Mod + P`, `Mod + E`, `Mod + S`, `Mod + H/J/K/L`, `Mod + Shift + H/J/K/L`, `Mod + 1..9`, `Mod + Shift + 1..9`, `Mod + MouseLeft/Right`, `Mod + WheelScrollDown/Up`). Individual `koma.bind(...)` definitions override matching chords in-place without discarding other defaults, and `koma.unbind(chord)` removes specific default bindings.
- **Appearance, shaders, and spring animations**: `koma.appearance({...})` exposes typed, default-backed configuration tables for `border` (`BorderConfig`), `focus_ring` (`FocusRingConfig`), `shadow` (`ShadowConfig`), `dim` / `brightness` (`DimConfig`), Dual-Kawase `blur` (`BlurConfig`), and critically damped spring `animations` (`AnimationConfig`).

## Repository structure

| Crate | Target | Source responsibility |
| --- | --- | --- |
| [koma-proto](https://github.com/komawm/koma/tree/main/crates/koma-proto) | Library | IPC schemas and framing, ancillary FD transport, capability tokens, epoch identifiers, event buffers. |
| [koma-model](https://github.com/komawm/koma/tree/main/crates/koma-model) | Library | Dwindle, scrolling/tabbed columns, deck/stash and floating workspaces, leases, rules, springs, frame clocks. |
| [koma-lua](https://github.com/komawm/koma/tree/main/crates/koma-lua) | Library | Configuration schema, Phodopus bridge, host operations, coroutine scheduling, macro recording, LuaLS type generation. |
| [koma-render](https://github.com/komawm/koma/tree/main/crates/koma-render) | Library | Smithay scene composition, shaders, blur, offscreen pools, capture overlays and protected-surface handling. |
| [koma-seat](https://github.com/komawm/koma/tree/main/crates/koma-seat) | Library | Virtual-seat held-input ledger and text-injection interface. |
| [koma](https://github.com/komawm/koma/tree/main/crates/koma) | Binary | Winit and TTY backend integration, Wayland handlers, IPC server, input dispatch, config reload and Xwayland integration. |
| [komactl](https://github.com/komawm/koma/tree/main/crates/komactl) | Binary | Configuration validation and reload, type generation, admin IPC, sandbox launcher. |
| [koma-mcp](https://github.com/komawm/koma/tree/main/crates/koma-mcp) | Binary | Stdio MCP adapter, agent IPC client, epoch handling and PNG encoding. |
| [koma-overview](https://github.com/komawm/koma/tree/main/crates/koma-overview) | Binary | Layer-shell companion, thumbnail/overview layout and daemon control. |

These nine crates belong to this Git repository. The [organization workspace](https://github.com/komawm/.github) also routes independent checkouts for canonical documentation, Jiku, Lua types and packaging. It uses no Git submodules. Phodopus is the existing shared upstream checkout, with its own identity and history.

## Development status and versions

The incubation baseline is `0.0.1`. Use the `0.0.x` line while interfaces and acceptance are still developing; the first formal release line will be `0.1.0`. A manifest version does not mark a release or authorize publication.

The target development toolchain is Rust `1.100.0`, with Rust 2024 edition. The [official stable announcement](https://blog.rust-lang.org/2026/11/12/Rust-1.100.0/) is the version reference; the [toolchain file](https://github.com/komawm/koma/blob/main/rust-toolchain.toml) owns the exact repository pin. Dependencies should use the latest verified compatible versions, with lockfile review. The fixed Smithay revision remains an explicit architecture/API compatibility exception until its migration is tested.

## Development

Use the pinned Rust toolchain and the repository [justfile](https://github.com/komawm/koma/blob/main/justfile). Run commands from the Koma checkout:

```bash
just --list
just fmt-check
just check-core
just test-core
```

Core checks do not exercise the compositor's display or hardware path. Cargo still resolves the workspace dependency graph, so disabled default features alone do not bypass an unavailable Git dependency. Read the [testing plan](https://github.com/komawm/docs/blob/main/docs/development/testing.md) before attempting a nested session, VM, or physical-seat test. Install Git hooks with `just hooks-install` when preparing a writable development clone.

Track work with per-repository CarryCtx; see [agent guidance](https://github.com/komawm/koma/blob/main/AGENTS.md). Temporary artifacts go under `/tmp/koma/`; retained logs, captures, and VM evidence go into the workspace's ignored `recording/` directory. Test evidence should identify the revision, command, backend and actual result.

## Specifications and examples

- [Koma architecture](https://github.com/komawm/docs/blob/main/docs/ARCHITECTURE.md), [IPC specification](https://github.com/komawm/docs/blob/main/docs/IPC_SPEC.md), and [Lua API specification](https://github.com/komawm/docs/blob/main/docs/LUA_API_SPEC.md).
- [Jiku architecture](https://github.com/komawm/docs/blob/main/docs/JIKU_ARCHITECTURE.md) and [Jiku configuration proposal](https://github.com/komawm/docs/blob/main/docs/JIKU_CONFIG_SPEC.md).
- [Example configurations](https://github.com/komawm/koma/tree/main/examples), [LuaLS definitions](https://github.com/komawm/koma-types/tree/main/types), and [session resources](https://github.com/komawm/koma/tree/main/resources).

The session installer writes system integration files and can affect login/session behavior. It is a separate deployment action after build and runtime acceptance; workspace initialization does not install or activate it.

## License

All Koma crates, including [koma-proto](https://github.com/komawm/koma/tree/main/crates/koma-proto), use [GPL-3.0-or-later](https://github.com/komawm/koma/blob/main/LICENSE). Preserve third-party notices and the independent licensing of [Phodopus](https://github.com/Xuepoo/phodopus).
