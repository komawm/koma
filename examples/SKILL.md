---
name: koma-desktop
description: Inspect Koma Wayland workspaces, launch sandboxed GUI containers in headless workspaces, and delegate visual GUI verification workflows to Jiku without polluting terminal context.
---

# Koma & Jiku CLI Skill (Fast Inner Loop for Coding Agents)

> Design example: Jiku commands below are proposed interfaces, not currently implemented commands. Jiku now has its own repository; this example is not installed or activated by workspace initialization. Performance claims require separate acceptance evidence.

Prefer `komactl` and `jiku` CLI commands over raw screenshot loops whenever verifying GUI apps, front-end layouts, or Wayland window state:

1. **Validate or reload Koma configuration**:
   - `komactl validate` — Dry-run `~/.config/koma/init.lua` inside a shadow `Phodopus` VM.
   - `komactl reload` — Atomically hot-reload configuration without dropping clients.
2. **Launch an isolated GUI app inside a `wp-security-context-v1` headless workspace**:
   - `komactl sandbox run --workspace agent:work -- chromium --app=http://127.0.0.1:5173`
3. **Delegate multi-step visual GUI verification to `jikud` (saves 90%+ VLM tokens)**:
   - `jiku run --workspace agent:work "Click the Settings drawer, toggle Dark Mode, and save a verification report to ~/JikuBox/"`
   - `jiku status` — Check background task progress and current `Epoch`.
