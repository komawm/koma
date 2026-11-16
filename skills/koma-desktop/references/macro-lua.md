# Macro Lua

A macro is a `Phodopus` Lua 5.4 function that runs atomically inside the
compositor. `koma_exec_macro` launches one by id, with arguments, and returns a
result — no screenshot loop, no per-step round trip, no model tokens.

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

- The epoch is checked once, at entry. Inside the macro the workspace is yours.
- A macro that hits a protected surface or a preempted workspace aborts; it does
  not retry internally.
- Results are returned as data. Report the macro id and args, not a narrative of
  what you believe it did.

## Authoring Rules

1. Read the type stubs: `komactl gen-types` emits LuaLS definitions for the
   exact ABI in use. Do not guess signatures.
2. Validate with `komactl validate` before reloading. It runs in a shadow VM and
   cannot touch the live session.
3. Keep macros idempotent where possible. A retried macro should not double-type
   text.
4. Never put secrets in macro params. Arguments are visible in configuration and
   in the call record.

## Recording

The compositor can record a human demonstration and denoise it into a macro.
Recorded macro content is untrusted input: review the generated Lua before
enabling it, exactly as you would review generated test code. A recorder that
captured a stray drag produces a macro that will keep making that mistake.
