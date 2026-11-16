# Coding Agent CLI Workflow

This is a design sample of the CLI-first path for terminal coding agents. The
installable skill now lives at
[`skills/koma-desktop/`](../skills/koma-desktop) and supersedes this page:

```bash
npx skills add komawm/koma --skill koma-desktop -g
```

Keep reading for the shape of a shell-driven loop, where the agent already has a
`bash` tool and only needs to reach the compositor occasionally.

## Prefer CLI Over Raw Screenshot Loops

When verifying GUI apps, front-end layouts, or Wayland window state, reach for
`komactl` and `jiku` before building a capture loop:

1. **Validate or reload Koma configuration**
    - `komactl validate` — dry-run `~/.config/koma/init.lua` inside a shadow
      `Phodopus` VM.
    - `komactl reload` — atomically hot-reload without dropping clients.
    - `komactl config-status` — print what is actually active.

2. **Launch an isolated GUI app in a security-context workspace**
    - `komactl sandbox run --workspace agent:work -- chromium --app=http://127.0.0.1:5173`

3. **Delegate multi-step visual verification to Jiku**
    - `jiku run --workspace agent:work "Click the Settings drawer, toggle Dark Mode, and save a verification report to ~/JikuBox/"`
    - `jiku status` — check task progress and the current `Epoch`.
    - `jiku run --workspace agent:work --dry-run "<goal>"` — plan without emitting
      input events.

Both binaries are separate packages: `komactl` ships with Koma, `jiku` with
[Jiku](https://github.com/komawm/jiku). Neither is required for terminal-only
work, and neither replaces your shell.

## Why This Shape

- No MCP schema cost and no image tokens until you explicitly capture.
- The agent's existing `bash` permission model stays the only execution surface.
- `jiku` output is JSON, so the agent can reason over a report instead of pixels.

The trade-off is that you lose the epoch-aware retry loop that `koma-mcp` gives
you for free. If the workflow is coordinate-heavy, attach the MCP server instead;
see [`skills/koma-desktop/references/input-primitives.md`](../skills/koma-desktop/references/input-primitives.md).
