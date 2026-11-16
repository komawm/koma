# Input Primitives

All input goes through the workspace's virtual seat. Nothing here touches the
human's physical devices or the shared clipboard.

## grid_225

`koma_capture_frame` with `overlay: "grid_225"` splits the frame into a 15x15
grid labelled `A1`..`O15` and draws the labels into the image. The model reads a
label and passes it back:

```text
koma_click { ws_id, expected_epoch, grid_225: "G8" }
```

Why this beats raw pixels:

- The label is what the model literally sees, so a mis-click is a mis-read, not a
  silent coordinate drift.
- Cell centres absorb small window resizes.
- Coordinates still work (`x`, `y`) for fine targeting inside a cell.

`grid_cells` comes back as `225` when the overlay was applied. If it is absent,
the overlay was not drawn and cell labels are meaningless.

## Pointer

| Tool                 | Purpose                                                                                          |
| -------------------- | ------------------------------------------------------------------------------------------------ |
| `koma_click`         | Click a cell or point. `button`, `clicks` (1–3), `modifiers`.                                    |
| `koma_drag`          | Smooth drag between two points. `duration_ms`, `modifiers`, `bow`, `overshoot`, `tremor`.        |
| `koma_path`          | Multi-waypoint spline (`catmull_rom` or `bezier`), up to 128 points, optional hold before/after. |
| `koma_wheel`         | Axis frame: `dx`/`dy` continuous, `discrete_x`/`discrete_y` for notch scrolling.                 |
| `koma_relative_move` | Unaccelerated relative motion for camera/3D control (`zwp_relative_pointer_v1`, locked pointer). |
| `koma_hit_test`      | Probe 5 clamped points around a target to detect occlusion. Does not mutate.                     |
| `koma_cursor_label`  | Draw a caption plus optional caret rectangle.                                                    |
| `koma_cursor_settle` | Fade the agent cursor out (`280 ms`) or hide it immediately.                                     |

Kinematic parameters (`bow`, `overshoot`, `tremor`) exist so a demonstration looks
human. They are not needed for correctness — leave them at defaults unless the
task is about recording a believable trace.

`koma_hit_test` is the cheap way to answer "is something covering my target?"
before spending a click and an epoch retry.

## Keyboard

```text
koma_key { ws_id, expected_epoch, chord: "Ctrl+S" }
```

Chords are `+`-joined with symbolic names (`Return`, `Tab`, `Escape`, arrows).
Modifiers are also accepted as a list on pointer tools, so you can
`Ctrl+click` without a key event.

Do not type prose as a key sequence. Use text injection.

## Text and MIME

```text
koma_inject_text { ws_id, expected_epoch, text: "..." }
koma_paste_mime  { ws_id, expected_epoch, mime_type: "text/tab-separated-values", data_utf8: "..." }
```

- `inject_text` bypasses Fcitx5/IBus, so CJK and emoji land correctly and no
  candidate popup steals focus.
- `paste_mime` is for structured content: TSV for spreadsheets, `text/html` for
  rich documents. It uses the workspace's isolated clipboard, not the human's.
- Never use the shared clipboard bridge (`allow_clipboard_bridge_from`) to move
  data between an agent workspace and the human's session. If a token allows it,
  it is still a privacy boundary.

## Timing

The compositor settles surfaces before delivering input, so most `sleep` calls
are unnecessary. When you do need one, do it in a macro
(`koma.sleep_ms`) rather than in your own tool loop — a macro holds its
transaction, a tool loop just costs round trips.

## Verification

Input returning success means the event was delivered. Confirm the effect:

```text
koma_capture_frame { ws_id, overlay: "grid_225", max_edge: 1024 }
```

Compare against your intent. A dialog that opened, a checkbox that changed, a
menu that closed — those are results. A non-error JSON response is not.
