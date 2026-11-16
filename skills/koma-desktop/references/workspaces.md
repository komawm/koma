# Workspaces

A Koma workspace owns a window stack, a lease, an `Epoch`, and one virtual seat.
Everything an agent does happens inside a workspace.

## Modes

`WorkspaceModeSpec` has four values:

| Mode       | Rendering                           | Epoch              | Who can set it                   |
| ---------- | ----------------------------------- | ------------------ | -------------------------------- |
| `headed`   | Physical output                     | Advances on change | Human session                    |
| `headless` | Off-screen, 0% idle GPU             | Advances on change | Agent with `can_create_headless` |
| `peek`     | Visible, read-only view             | **Preserved**      | Agent                            |
| `takeover` | Physical output under agent control | Re-fenced          | Admin channel only               |

- `headless` is the default for agent work. It renders on demand, so an idle
  workspace costs nothing.
- `peek` is for observation of a human workspace. It deliberately does not
  advance the epoch, because nothing about the workspace state changes.
- `takeover` is an admin operation. An agent token cannot request it, and it is
  not a workaround for `4030`.

## Creating a Workspace

```text
koma_ws_create { name: "agent:work", width: 1920, height: 1080 }
```

`name` is required; the defaults are a 1920x1080 deck. Creation returns the
workspace id and its initial write lease. Treat the id as opaque — never
hardcode one into a reusable procedure.

Configuration can also declare workspaces in `~/.config/koma/init.lua`, which is
the right place for a stable dev workspace:

```lua
koma.workspace({ name = "agent:work", presentation = "headless", layout = "deck" })
```

## Deck and Stash

Each workspace holds a `DeckStashStack`:

- **Deck** — the active, full-viewport window. One window is on the deck.
- **Stash** — background windows, kept alive but not rendered.

`koma_win_activate { ws_id, win_slot }` promotes a Stash window onto the Deck.
This is how you switch apps without losing their state, and it is why "the window
disappeared" usually means "it moved to Stash", not "it crashed".

`scene_inventory` reports both stacks with their slots and generations. Slot
numbers are not stable across window churn — re-inventory rather than caching.

## Leases

A lease is exclusive write access to a workspace. Two agents cannot hold one at
the same time.

- Creation grants the initial lease.
- `ERR_LEASE_HELD_BY_OTHER` means someone else is driving. Do not force it;
  release yours or pick another workspace.
- Dropping a lease without releasing held input is a bug the compositor guards
  against, but a crashed agent can still leave a workspace wedged. If input
  feels stuck, re-inventory and check `held_inputs` before sending more events.

## Epoch

The epoch is the observation fence. `capture.frame` records it; mutations assert
it.

- `4090` means your number is stale. The action was refused, not applied.
- `peek` does not advance it, so a peeked epoch stays valid for reads but is not
  a licence to mutate.
- Two consecutive `4090` responses mean the workspace is churning faster than
  you can act. Stop pixel targeting and re-plan.

## Lifecycle

| Level          | Operations                                                  |
| -------------- | ----------------------------------------------------------- |
| `observe_only` | `scene.*`, `capture.*`, read subscriptions                  |
| `control`      | `lease.*`, `win.*`, `vseat.*`, `lua.exec_tx` (epoch-fenced) |
| `owner`        | `ws.set_frame_policy`, `ws.spawn_bound`, `ws.destroy`       |

Destroying a workspace kills its windows. Do that only for workspaces you
created, and only when the human did not ask to inspect the result afterwards.

## Configuration

```bash
komactl validate        # dry-run ~/.config/koma/init.lua in a shadow VM
komactl reload          # atomic hot reload, clients keep running
komactl config-status   # what is actually active
komactl gen-types       # emit LuaLS stubs for the current ABI
```

`validate` is offline and safe. `reload` is a live mutation of the human's
session — do not call it unless the task is about configuration.
