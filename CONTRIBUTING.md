# Contributing to Koma

Thank you for contributing to Koma (`komawm/koma`). This repository owns Koma's
nine Rust crates (`koma-proto`, `koma-model`, `koma-lua`, `koma-render`,
`koma-seat`, `koma`, `komactl`, `koma-mcp`, and `koma-overview`), example Lua
configurations, and session integration resources.

Start by reading [AGENTS.md](https://github.com/komawm/koma/blob/main/AGENTS.md),
the [organization contribution guide](https://github.com/komawm/.github/blob/main/CONTRIBUTING.md),
and the [current-state inventory](https://github.com/komawm/docs/blob/main/docs/development/current-state.md).

## Repository Scope

Keep changes in their owning repository:

- Compositor, layout model, IPC protocol, Lua bridge, renderer, seat, CLI, MCP,
  and overview crates: [komawm/koma](https://github.com/komawm/koma)
- Canonical architecture, IPC, Lua API, and development specifications:
  [komawm/docs](https://github.com/komawm/docs)
- Versioned LuaLS type definitions: [komawm/koma-types](https://github.com/komawm/koma-types)
- Desktop orchestration companion: [komawm/jiku](https://github.com/komawm/jiku)
- Distribution packaging and CD: [komawm/packaging](https://github.com/komawm/packaging)
- Organization templates and workspace governance: [komawm/.github](https://github.com/komawm/.github)

[Phodopus](https://github.com/Xuepoo/phodopus) is an independent upstream Cargo
dependency. Koma does not use Git submodules; do not copy Phodopus sources into
this repository or commit local path overrides in `Cargo.toml`.

## Prerequisites

- **Rust `1.100.0`** (Rust 2024 edition), pinned in
  [rust-toolchain.toml](https://github.com/komawm/koma/blob/main/rust-toolchain.toml)
  with `rustfmt` and `clippy`
- **`just`** — task runner sharing gates between local development and CI
- **Validation tools** — `python3`, `shellcheck`, `actionlint`, `cargo-deny`,
  `lefthook`, and `carryctx`
- **Linux native build dependencies** (also available via `nix develop` using
  [flake.nix](https://github.com/komawm/koma/blob/main/flake.nix)):
  `pkg-config`, `libudev`, `libinput`, `libseat` (`seatd`), `libgbm` / `mesa`,
  `libegl` / `libgles2`, `libdrm`, `wayland`, `libxkbcommon`, `pixman`, `dbus`,
  and `fontconfig`

## Setup

Clone the repository directly and optionally install local Git hooks:

```bash
git clone https://github.com/komawm/koma.git
cd koma
just hooks-install
```

In grouped multi-repository checkouts, use a repository-local `CARGO_TARGET_DIR`
(such as the workspace `.targets/` cache) rather than filling `/tmp`.

## Quality Gates

All standard gates are defined in the
[justfile](https://github.com/komawm/koma/blob/main/justfile):

```bash
just --list        # list available recipes
just check         # infra-check + fmt-check + typecheck + clippy + test
just infra-check   # metadata validation + actionlint + shellcheck
just fmt-check     # cargo fmt --all -- --check
just check-core    # core library crates without default Smithay/TTY features
just test-core     # core library unit tests
just typecheck     # cargo check --workspace --all-targets --locked
just clippy        # cargo clippy --workspace --all-targets --locked -- -D warnings
just test          # cargo test --workspace --all-targets --locked
just deny          # cargo deny --locked check (advisories, bans, licenses, sources)
just gen-types     # emit LuaLS definitions to stdout via komactl
```

Passing `just check` and `just deny` covers metadata, linting, supply-chain
policy, compilation, and unit tests only. It does **not** establish nested
`--winit` rendering, isolated VM behavior, bare-metal DRM/KMS scanout, physical
`libinput`, or security acceptance. Follow the
[testing plan](https://github.com/komawm/docs/blob/main/docs/development/testing.md)
for display and VM validation, and never replace an active desktop session or
alter host services during routine development.

## Workflow, CarryCtx, and Pull Requests

1. Register a named CarryCtx identity, start a session, and claim or create a
   scoped `KOM-XXXX` task before multi-step edits (see the
   [CarryCtx catalog](https://github.com/komawm/koma/blob/main/.carryctx/README.md)).
2. Work in an isolated branch and Git worktree (for example `task/KOM-XXXX` or
   `carryctx/kom-XXXX-*`). Keep commits coherent and scoped to the task.
3. Place disposable scratch artifacts in `/tmp/koma/` and retained local logs
   or VM evidence in workspace-ignored `recording/`. Never commit capability
   tokens, runtime sockets, raw CarryCtx databases, ISO images, VM disks, or
   private captures.
4. Keep protocol changes synchronized across `koma-proto`, compositor handlers,
   `komactl`, `koma-mcp`, and the
   [IPC specification](https://github.com/komawm/docs/blob/main/docs/IPC_SPEC.md).
   Keep Lua schema changes synchronized with `komactl gen-types` and
   [komawm/koma-types](https://github.com/komawm/koma-types).
5. In pull requests, link the GitHub Issue and CarryCtx task ID, list the exact
   gates run and their results, distinguish headless/unit checks from nested,
   VM, or physical GPU observations, apply `type:*`, `area:*`, and `platform:*`
   labels, and resolve blocking review or CI findings before merge.

## Licensing

All nine Koma crates, including `koma-proto`, are licensed under
`GPL-3.0-or-later` and share the single root
[LICENSE](https://github.com/komawm/koma/blob/main/LICENSE). Do not add
per-crate `LICENSE*` files or `LICENSES/` directories. Third-party dependency
licenses are enforced by `deny.toml`.

## Reporting Issues and Vulnerabilities

Report bugs and feature requests through
[GitHub Issues](https://github.com/komawm/koma/issues). Do **not** report
security vulnerabilities publicly; follow
[SECURITY.md](https://github.com/komawm/koma/blob/main/SECURITY.md).
