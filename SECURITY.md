# Security Policy

## Supported Versions

Koma is currently on the `0.0.x` incubation baseline (`0.0.1` development
prerelease). No formal `0.1.0` product release exists yet, and no released
artifact receives security support guarantees.

| Version | Supported |
| --- | --- |
| `0.0.x` | No (development prerelease; best-effort fixes on `main`) |

## Reporting a Vulnerability

Report suspected security vulnerabilities privately through a
[GitHub Security Advisory](https://github.com/komawm/koma/security/advisories/new).

Do not report security vulnerabilities via public GitHub issues, pull
requests, discussions, or chat channels.

When reporting, include as much of the following as possible:

- Affected crate, component, or file (`koma`, `koma-proto`, `koma-model`,
  `koma-lua`, `koma-render`, `koma-seat`, `komactl`,
  `koma-overview`, or session resources)
- Minimal reproduction steps or proof of concept
- Expected versus observed behavior and security impact
- Environment details (distribution, kernel, backend such as headless,
  `--winit`, `--tty`, or VM, and Rust/dependency versions)

Never include credentials, capability tokens, active runtime sockets, raw
CarryCtx state databases, or private screen captures in reports.

## Disclosure Policy and Response Expectations

- Reports are acknowledged and triaged by maintainers, with an initial
  response targeted within a reasonable timeframe after the advisory is filed.
- Accepted issues are investigated, patched, and coordinated with the reporter
  prior to any public disclosure.
- Reporters are credited in release notes unless anonymity is requested.
- Public details are published only after a fix or mitigation is available.

## Security Boundaries and Current Scope

This policy describes vulnerability reporting and disclosure process only; it
does not claim that security controls or sandbox boundaries are independently
accepted. Current boundaries include Wayland client surfaces, `koma-proto` IPC
framing and `SCM_RIGHTS` FD transport, capability tokens and epoch fencing,
`koma-lua` Phodopus fuel/memory limits, `koma-render` protected-surface capture
blackout, `koma-seat` preemption (`Mod+G`), and `komactl` / `koma mcp`
boundaries.

For normative design requirements, current implementation status, and review
criteria, consult [komawm/docs](https://github.com/komawm/docs), the
[current-state inventory](https://github.com/komawm/docs/blob/main/docs/development/current-state.md),
and Koma's [security review rules](https://github.com/komawm/koma/blob/main/.carryctx/rules/security.md).
