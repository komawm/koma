# Koma repository guidance

## Scope and authority

This repository owns Koma's nine Rust crates, Koma configuration examples and session resources. Canonical specifications belong to [koma-docs](https://github.com/komawm/docs), Jiku belongs to [jiku](https://github.com/komawm/jiku), and the LuaLS snapshot belongs to [koma-types](https://github.com/komawm/koma-types). Read the [workspace guidance](https://github.com/komawm/.github/blob/main/workspace/AGENTS.md), [current-state inventory](https://github.com/komawm/docs/blob/main/docs/development/current-state.md), and relevant specifications before editing. The [repository descriptor](https://github.com/komawm/koma/blob/main/repo.toml) owns identity; the [workspace map](https://github.com/komawm/.github/blob/main/workspace/workspace.toml) owns checkout routing.

Keep the nine Koma crates within this Git repository. There are no submodules. Phodopus is an independent upstream repository and a Cargo dependency; do not copy its implementation here or change its ABI incidentally. Jiku is an independent repository with its own CLI and daemon; do not duplicate or silently re-implement its orchestration here. Do not describe proposed or unmeasured behavior as shipped, secure, portable, or latency-verified.

## Work lifecycle

Run Git, CarryCtx and quality commands inside this checkout, never at the umbrella workspace root. CarryCtx state is local to the repository Git common directory. Register a named identity, start a session, inspect `carryctx resume --compact` and task records, and create scoped tasks before multi-step changes. Consult command-specific `--help` for the installed CarryCtx version.

For delegated work, record task dependencies and file scopes; use separate worktrees after the first commit. Before the initial commit, keep shared-checkout scopes disjoint. Persist progress and decisions, checkpoint before stopping, and end the session. The coordinator reads durable state and independently verifies changes before accepting task completion. Keep open defects in explicit follow-up tasks. Never publish raw CarryCtx state; publish only an intentionally redacted export when sharing is authorized.

## Versions, dependencies and licensing

The incubation baseline is `0.0.1` on the `0.0.x` line; the first formal release line will be `0.1.0`. Rust `1.100.0` and Rust 2024 edition are the target baseline. Use the exact toolchain file; an installed older compiler used for structural probes is not target-toolchain validation. Keep dependency upgrades on verified compatible versions, review the lockfile, and record intentional exceptions such as the existing fixed Smithay architecture/API pin until its migration is tested.

All Koma crates, including `koma-proto`, use `GPL-3.0-or-later` and inherit the workspace license metadata. Keep one root `LICENSE` and preserve Phodopus and other third-party licensing and provenance. The installable agent skill under `skills/` is MIT licensed and carries its own `LICENSE` file, because the skills installer copies only that directory.

Read the [CarryCtx rule catalog](https://github.com/komawm/koma/blob/main/.carryctx/README.md) for local delivery, documentation, security, performance and review policy. Local initialization does not require or authorize remote Issues, PRs, merges, snapshot publication or releases. When remote delivery is explicitly in scope, use labels/milestones and independent CodeRabbit review as described by the [delivery workflow](https://github.com/komawm/koma/blob/main/.carryctx/workflows/issue-to-merge.md).

## Implementation and validation

Use the pinned [toolchain](https://github.com/komawm/koma/blob/main/rust-toolchain.toml) and [justfile](https://github.com/komawm/koma/blob/main/justfile) for local and CI commands. Begin with `just --list`, then relevant metadata, formatting, core tests and full feature checks. A passing core suite does not establish Phodopus integration, Smithay compilation, rendering, session lifecycle, security isolation or hardware behavior. Report preexisting failures with their commands and logs. Do not weaken CI to make an unresolved baseline appear green.

Keep protocol changes coordinated across `koma-proto`, compositor handlers, `komactl`, `koma-mcp` and the [IPC specification](https://github.com/komawm/docs/blob/main/docs/IPC_SPEC.md). Preserve epoch/capability checks, input release symmetry, protected-capture behavior and bounded host execution. Treat timing and isolation guarantees as acceptance requirements requiring measurements and adversarial tests.

## Local environment and artifacts

The development host is Arch Linux using Hyprland and has a dedicated GPU. Device access inside an agent environment must be checked independently: the initialization environment exposed neither `/dev/kvm` nor `/dev/dri`. QEMU availability or an ISO image does not establish KVM/GPU access. Use `ISO_PATH` for the local ISO inventory; do not hardcode one host's mount into portable configuration.

Follow the [testing plan](https://github.com/komawm/docs/blob/main/docs/development/testing.md): pure/core checks, explicit nested Winit tests, disposable VM tests, then separately planned physical-seat tests. Do not stop Hyprland, activate `koma-session`, start TTY mode on the active seat, change system session files or assign a GPU merely to initialize the workspace.

Disposable files belong under `/tmp/koma/`. Retained logs, screenshots, disk overlays and experiment notes belong in the workspace's Git-ignored `recording/` directory; the directory is not a repository or CarryCtx project. Keep credentials, runtime sockets and VM images out of commits. Link repository documentation with canonical HTTPS URLs so it remains portable across clones; literal command paths and user configuration locations are fine where needed to explain usage.
