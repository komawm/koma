# Macro Lua

A macro is a `Phodopus` Lua 5.4 function that runs inside the compositor. It can
suspend at host operations such as `ctx:drag` or `ctx:wait_settle`, so
`koma_exec_macro` launches it and returns the executor status and epoch — not the
macro's final result. What you buy is no screenshot loop, no per-step round trip,
and no model tokens.

## When a Macro Beats a Tool Loop

Use a macro when the sequence is:

- **Repetitive.** The same 12 clicks happen every run.
- **Latency-sensitive.** Settle waits and inter-event timing belong in the
  compositor, not on a network round trip.
- **Already recorded.** A human demonstrated it once; replay is deterministic.

Keep a tool loop when the path depends on what the screen shows — a macro cannot
see, and hardcoding a branch into one is how brittle automations are born.

## Shape

Macros are declared in `~/.config/koma/init.lua`:

```lua
koma.macro.define({
    id = "wps.drag_indent",
    app_id = "^wps$",
    description = "Drags the first-line indent ruler",
    params = { delta_px = { type = "int", default = 64 } },
    run = function(ctx, args)
        ctx:assert_app("^wps$")
        ctx:drag({
            button = "left",
            from = { anchor = "top-left", x = 240, y = 148 },
            to   = { anchor = "top-left", x = 240 + args.delta_px, y = 148 },
            hold_before_move_ms = 60,
            kinematics = { profile = "minimum_jerk", duration_ms = 120, arc_curvature = 0.0 },
            hold_before_release_ms = 30,
        })
        ctx:text("Heading 1", { strategy = "auto" })
    end,
})
```

Fields that matter:

| Field    | Meaning                                                                                                        |
| -------- | -------------------------------------------------------------------------------------------------------------- |
| `id`     | Dotted identifier used by `koma_exec_macro` and `koma.act.macro.run`.                                          |
| `app_id` | Regex guard. `ctx:assert_app` enforces it at runtime, so a macro cannot silently run against the wrong window. |
| `params` | Typed arguments with defaults (`int`, `string`, etc.). Validate, do not assume.                                |
| `limits` | Fuel and heap ceiling for the run.                                                                             |
| `run`    | `function(ctx, args)`; the only entry point.                                                                   |

## Context API

`ctx` is the host ABI bound to the macro's workspace:

- `ctx:assert_app(regex)` — fail fast if the focused app does not match.
- `ctx:drag{...}` — pointer drag with anchors and kinematics.
- `ctx:text(str, { strategy = "auto" })` — text injection; `auto` picks the
  best of the three injection tiers.
- `ctx:wait_settle()` / `koma.sleep_ms(ms)` — suspension points that park the
  coroutine without blocking the frame loop.

Coordinates are `0-based` for pixels, anchors, monitors, and timestamps.
Collection tables use idiomatic Lua `1-based` iteration. Mixing these up is the
most common macro bug.

## Running

```text
koma_exec_macro { ws_id, expected_epoch, macro_id: "wps.drag_indent", args: { delta_px = 32 } }
```

The call returns one of the executor modes plus the epoch it was launched
against — typically `HostSuspended` when the macro immediately parks on a host
operation. It never returns the macro's return value.

Completion rules you must plan around:

- A suspended macro resumes when its timer or damage-settle condition fires. The
  scheduler re-checks the workspace epoch at every resume; a changed epoch
  cancels the macro with `ERR_STALE_EPOCH`.
- Human takeover (`Mod+G`) and preemption cancel live macro coroutines for the
  workspace.
- Host operations that already ran stay applied. Cancellation is not a rollback.
- Read completion from the macro's own reported result or from a fresh
  observation, never from the `koma_exec_macro` response.

The mutation gate checks the epoch at entry. That is not enough on its own: a
macro that suspends outlives the check, which is why the resume path re-validates.

## Authoring Rules

1. Read the type stubs: `komactl gen-types` emits LuaLS definitions for the
   exact ABI in use. Do not guess signatures.
2. Validate with `komactl validate` before reloading. It runs in a shadow VM and
   cannot touch the live session.
3. Keep macros idempotent where possible. A retried macro should not double-type
   text — and because cancellation does not roll back, a partial run is a real
   outcome.
4. Never put secrets in macro params. Arguments are visible in configuration and
   in the call record.

## Recording

The compositor can record a human demonstration and denoise it into a macro.
Recorded macro content is untrusted input: review the generated Lua before
enabling it, exactly as you would review generated test code. A recorder that
captured a stray drag produces a macro that will keep making that mistake.
