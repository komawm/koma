---
name: koma-desktop
description: >
    Inspect and drive Koma Wayland workspaces: capture labelled screenshots,
    inject pointer and keyboard input into isolated virtual seats, run sandboxed
    GUI applications, and execute recorded Lua macros. Load when a task needs a
    mockup of a real compositor instead of a headless browser.
license: MIT
metadata:
    author: Xuepoo
    version: "0.0.1"
    requires: "koma >= 0.0.1"
---

# Koma Desktop

Koma is an agent-native Wayland tiling compositor. Every workspace has its own
virtual seat (`vSeat`), so an agent can click, type, and scroll without touching
the human's physical input devices or clipboard.

## When to Apply

Use Koma when the task needs a real compositor surface:

- Verifying a native application's window behaviour, focus, or layout.
- Driving an app that has no test harness and no accessible tree worth using.
- Reproducing a window-management bug (tiling, floating, fullscreen, focus).
- Running a GUI program in a disposable workspace while you keep working.

Do **not** use Koma for file edits, builds, or tests. Koma exposes no shell tool,
and it should not gain one: agents already have a shell, and a compositor that
can spawn arbitrary processes is a privilege escalation path.

If the task is goal-level ("open settings and enable dark mode"), prefer Jiku.
Koma is the layer underneath it.

## Prerequisites

```bash
komactl inventory      # workspaces, epochs, Deck/Stash windows
komactl config-status  # validated configuration and active bindings
```

- A running Koma session. `komactl` and `koma-mcp` both need
  `$XDG_RUNTIME_DIR/koma/$KOMA_INSTANCE_SIGNATURE/`.
- For `koma-mcp`, a capability token: `komactl token mint <label>` exports
  `KOMA_CAPABILITY_TOKEN`. Never use `--auto-mint-admin` for an agent host.

## Two Entry Points

| Entry point | Use when                                                                |
| ----------- | ----------------------------------------------------------------------- |
| `komactl`   | Terminal-first agent: you already have a shell and want no schema cost. |
| `koma-mcp`  | Your host is an MCP client and needs PNG content blocks.                |

Both talk to the same `koma-agent.sock`. Pick one per task; do not interleave
mutations from both against the same workspace.

## The Core Loop

1. **Inventory.** `koma_scene_inventory` / `komactl inventory`.
2. **Create or select a workspace.** Prefer a `Headless` workspace
   (`koma_ws_create`) over a `Headed` one holding the human's windows.
3. **Observe.** `koma_capture_frame` with `overlay: "grid_225"` returns a PNG
   and records a fresh `Epoch`.
4. **Act.** Every mutation carries `expected_epoch`.
5. **Verify.** Re-capture. A non-error response means the RPC was accepted, not
   that the application did what you wanted.

## Error Triage

| Signal                       | Meaning                                                                            | Response                                                                   |
| ---------------------------- | ---------------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| `4090 ERR_STALE_EPOCH`       | Workspace changed since your observation; the action was aborted before execution. | Inspect the attached fresh PNG, then retry once with `new_observed_epoch`. |
| `4091 ERR_HUMAN_PREEMPTED`   | The human took the workspace (`Mod+G`).                                            | Stop and ask. Never retry.                                                 |
| `4032 ERR_PROTECTED_SURFACE` | Sensitive-surface blackout.                                                        | Stop. Report the intent, not the content.                                  |
| `4030 ERR_CAPABILITY_DENIED` | Token scope or expiry.                                                             | Ask the human for a new token.                                             |
| `ERR_SETTLE_TIMEOUT`         | The surface never settled.                                                         | Re-inventory; the window may be animating or gone.                         |

## Isolation Rules

- `Headless` workspaces render off-screen and cost no idle GPU; use them for
  agent work by default.
- A physical monitor workspace belongs to the human. Agent tokens cannot hold
  `Control`/`Owner` over one while it is `HumanPreempted`.
- Free text goes through `koma_inject_text` or `koma_paste_mime`, never through
  synthesized per-character key events.
- A recorded macro (`koma_exec_macro`) runs atomically and costs no tokens; the
  reference covers when that beats a tool loop.

## Where the Details Live

- `references/workspaces.md` — Headed vs Headless, Deck/Stash, leases, epochs.
- `references/input-primitives.md` — `grid_225` targeting, pointer/button/wheel
  calls, text and MIME injection.
- `references/macro-lua.md` — `koma_exec_macro`, macro shape, type stubs.

## Reporting

Report the workspace id, the epoch you acted on, and the verification frame. For
example: "created `agent:work` (1920x1080 Headless), clicked `G8` at epoch 4,
verification capture shows the dialog closed" — not "clicked the button".
