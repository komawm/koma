# Bundled Koma Agent Skill

This directory ships the installable agent skill for Koma. The design sample in
[`../examples/coding-agent-cli.md`](../examples/coding-agent-cli.md) remains as a shorter CLI-first reference, but
the installable skill is this one.

## Install

Requires Node.js 22.20 or later.

```bash
# Install for every detected agent (global)
npx skills add komawm/koma --skill koma-desktop -g

# Install for one agent, in the current project
npx skills add komawm/koma --skill koma-desktop -a claude-code

# List what this repository publishes
npx skills add komawm/koma --list
```

## Layout

```text
skills/
└── koma-desktop/
    ├── SKILL.md          # thin router: when to use, core loop, error triage
    ├── LICENSE           # MIT (the repository root LICENSE stays GPL)
    └── references/
        ├── workspaces.md
        ├── input-primitives.md
        └── macro-lua.md
```

The catalog layout (`skills/<name>/SKILL.md`) is what the
[Skills CLI](https://github.com/vercel-labs/skills) discovers. `name` in the
frontmatter must equal the directory name.

## Licensing

The skill ships under **MIT** so it can be copied into any agent workspace. The
per-skill [`LICENSE`](koma-desktop/LICENSE) travels with the directory — the
installer copies only that folder, and the repository root
[`LICENSE`](../LICENSE) is `GPL-3.0-or-later`.

## Relationship to Jiku

Koma is the compositor layer: workspace inventory, capture, and input. Goal-level
desktop orchestration lives in Jiku, which has its own skill:
`npx skills add komawm/jiku --skill jiku-desktop-orchestration -g`. Install both
only when the agent actually needs to plan a desktop goal; a terminal-first agent
usually needs neither.
