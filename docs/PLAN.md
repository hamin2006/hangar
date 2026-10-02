# hangar — plan

One terminal app that answers "what's going on with my dev machine?":

| Tab | Question it answers | Data source |
| --- | --- | --- |
| **Projects** | Which projects do I have, what state is git in, is a dev server running, did the last deploy work? | Filesystem scan + `git` + `.vercel/project.json` |
| **Ports** | What is listening on which port, who started it, and can I kill it? | `lsof` (macOS/Linux) or `/proc/net/tcp*` (Linux) + `sysinfo` |
| **Deploys** | What's building, what failed, across Vercel and GitHub Actions? | Vercel REST API + GitHub REST API |

The tabs cross-reference each other: a port is attributed to the project whose folder the process runs in,
a project shows its running dev server and latest deploy, and a deploy is grouped under its project.

## Architecture

```
main.rs ── CLI (clap) ── `hangar` (TUI) | projects | ports | deploys | kill <port> | config
lib.rs
├─ config      ~/.config/hangar/config.toml (all optional, safe defaults)
├─ util        commands with hard timeouts, time/size formatting, ANSI stripping
├─ projects    scan roots → detect kind & dev command → git status/log/remote → vercel link
├─ ports       lsof / procfs parsers → group by (pid, port) → enrich via sysinfo → kill with escalation
├─ deploys     token discovery (env → config → CLI credentials) → Vercel + GitHub clients → unified model
├─ runner      start dev servers detached in their own process group, logs to the state dir, stop by group
└─ app         state + key handling + rendering (ratatui) + background workers (std threads + mpsc)
```

### Threading
The UI thread only renders and handles keys. Three workers refresh projects (15 s), ports (2 s) and deploys
(30 s) on their own timers, or immediately when asked, and send results over a channel. Slow user actions
(kill with escalation, deploy, start a server) run on short-lived threads and report back the same way.

## Robustness rules
- Every external command runs with a timeout, null stdin and `GIT_TERMINAL_PROMPT=0`, so nothing can hang the UI.
- Every HTTP call has a global timeout. Failures keep the last good data and mark it stale with the error.
- Expired Vercel CLI tokens are refreshed by running `vercel whoami` once, then retried.
- The terminal is restored on exit, on error and on panic.
- Killing refuses PID ≤ 1 and hangar itself, sends SIGTERM first and only escalates to SIGKILL on request.
- Dev servers run in their own process group so stopping them also stops the child (e.g. `npm` → `vite`).
- Parsers are pure functions tested against fixtures. Rendering is tested at tiny and huge terminal sizes.

## Milestones (each pushed to GitHub)
1. Skeleton, CLI, config, utilities, CI.
2. Projects scanner + `hangar projects`.
3. Ports scanner + kill + `hangar ports` / `hangar kill`.
4. Deploys (Vercel + GitHub Actions) + `hangar deploys`.
5. Dev server runner + logs.
6. TUI with all three tabs, actions, filter, help.
7. Hardening: render tests, integration tests, README.
