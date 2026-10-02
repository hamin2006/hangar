# ⛭ hangar

A terminal dashboard for your dev machine. One screen answers "what's going on with my projects?":

- **Projects**: every repo in your code folders, its git state, whether its dev server is running and how its last deploy went.
- **Ports**: everything listening on a TCP port, which project it belongs to, and a safe way to kill it.
- **Deploys**: Vercel deployments and GitHub Actions runs across all your projects, refreshing live.

```
 ⛭ hangar   1 Projects 34  2 Ports 1  3 Deploys 31                         ● 1 serving  ◐ 1 building  ✗ 4 failing
╭ Projects ───────────────────────────────────────────────────────────╮╭ Details ─────────────────────────────────╮
│   Name                 Kind   Branch   Changes     Sync  Last Deploy ││Wayfarer  vite                            │
│▌● Wayfarer             vite   main     clean       ✓     14h  ✓ 14h  ││~/Desktop/Projects/Wayfarer               │
│   hangar               rust   main     clean       ✓     2m   ◐ 1m   ││● npm run dev pid 60997 · up 3m           │
│   LP-Website           next   main     35 changed  ✓     1d   ✗ 21h  ││  ↳ http://localhost:5173  node           │
╰─────────────────────────────────────────────────────────────────────╯╰──────────────────────────────────────────╯
 s start/stop  o edit  l logs  u open app  b repo  D deploy  / filter  r refresh  ? help  q quit        updated now
```

## Install

Requires Rust 1.85+ and macOS or Linux.

```bash
cargo install --git https://github.com/hamin2006/hangar
# or, from a clone:
cargo install --path .
```

Then run `hangar`.

## The dashboard

| Key | Everywhere |
| --- | --- |
| `1` `2` `3` / `Tab` | switch tab |
| `j` `k` / arrows, `g` `G` | move, top, bottom |
| `/` | fuzzy filter (`Esc` clears) |
| `r` / `R` | refresh this tab / everything |
| `?` | all keys |
| `q` | quit |

| Key | Projects |
| --- | --- |
| `s` | start / stop the dev server (`npm run dev`, `cargo run`, …) in the background |
| `l` | live dev-server logs |
| `o` / `Enter` | open in your editor (`$VISUAL`, `$EDITOR` or `code`; terminal editors take over the screen) |
| `u` | open the running app in the browser |
| `b` / `v` | open the GitHub repo / latest deployment |
| `f` `t` `c` | reveal in Finder · open a terminal there · copy the path |
| `D` | deploy to Vercel production (asks first) |

| Key | Ports |
| --- | --- |
| `x` / `X` | stop (SIGTERM) / force-kill (SIGKILL), with confirmation |
| `o` | open `http://localhost:<port>` |
| `p` | jump to the owning project |
| `a` | include system services and desktop-app helpers |

| Key | Deploys |
| --- | --- |
| `o` / `i` | open the deployment or run / Vercel's build inspector |
| `p` `c` | jump to project · copy URL |

## Scriptable commands

Everything the dashboard shows is also available as plain text or JSON:

```bash
hangar projects [--json]          # projects with git state
hangar ports [--all] [--json]     # listening ports
hangar deploys [--json]           # Vercel + GitHub Actions
hangar kill 3000 [--force]        # stop whatever is on :3000
hangar start wayfarer             # start a dev server in the background
hangar logs wayfarer -n 50        # its output
hangar stop wayfarer              # stop it and everything it spawned
hangar config [--init]            # show / create the config file
```

`--root <dir>` (repeatable) scans other folders instead of the configured ones.

## How it works

- **Projects** are folders containing `.git`, `package.json`, `Cargo.toml`, `pyproject.toml`, `requirements.txt`, `go.mod` or `deno.json`, found up to two levels below each root (`~/Desktop/Projects`, `~/Projects`, `~/code`, `~/dev`, `~/src`… whichever exist). Git state comes from `git status --porcelain=v2` and `git log`.
- **Ports** come from `lsof` (macOS) or `/proc/net/tcp*` (Linux), enriched with process details. A port belongs to a project when its process runs inside that project's folder. Desktop-app helpers and system daemons are hidden until you press `a`.
- **Deploys** use the Vercel and GitHub REST APIs. Tokens are read from `$VERCEL_TOKEN` / `$GITHUB_TOKEN` (or the config file), falling back to your `vercel login` and `gh auth login` sessions, so usually there's nothing to set up. A project shows Vercel deploys once you've run `vercel link` in it.
- **Dev servers** started by hangar run in their own process group with output in `~/.local/state/hangar/logs/`. They keep running after you quit hangar, and stopping one stops its children too (e.g. `npm` → `vite`).

## Configuration

Optional. `hangar config --init` writes a commented starter file to `~/.config/hangar/config.toml`:

```toml
roots = ["~/Desktop/Projects", "~/code"]
max_depth = 2
ignore = ["node_modules", "target", "dist"]
editor = "code"
show_all_ports = false

[refresh]
ports_secs = 2
projects_secs = 15
deploys_secs = 30
```

A broken config never stops hangar: it prints a warning and uses defaults.

## Robustness

- Every external command (`git`, `lsof`, `vercel`, `gh`) runs with a timeout and no stdin, so nothing can hang the UI or prompt for a password.
- Data refreshes on background threads. If a source fails (offline, rate-limited, expired login), the last good data stays on screen next to the error.
- The terminal is restored on quit, on error and on panic.
- Kills refuse PID ≤ 1 and hangar itself, always ask first in the dashboard, and only escalate to SIGKILL when you choose force.

## Development

```bash
cargo test            # unit tests (parsers, rendering at many sizes) + end-to-end CLI tests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

CI runs all three on macOS and Linux. Design notes are in [docs/PLAN.md](docs/PLAN.md).

## License

MIT
