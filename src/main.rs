use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hangar::config::{self, Config};
use hangar::runner::{self, Runner};
use hangar::{deploys, ports, projects, util};

#[derive(Parser)]
#[command(
    name = "hangar",
    version,
    about = "Projects, ports and deploys for your dev machine, in one terminal dashboard"
)]
struct Cli {
    /// Scan these folders instead of the configured roots (repeatable).
    #[arg(long = "root", global = true, value_name = "DIR")]
    roots: Vec<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List projects with their git state.
    Projects {
        #[arg(long)]
        json: bool,
    },
    /// List listening TCP ports and the processes behind them.
    Ports {
        /// Include system services and other users' processes.
        #[arg(long, short)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Recent Vercel deployments and GitHub Actions runs for your projects.
    Deploys {
        #[arg(long)]
        json: bool,
    },
    /// Stop whatever is listening on a port (SIGTERM, then SIGKILL with --force).
    Kill {
        port: u16,
        #[arg(long, short)]
        force: bool,
    },
    /// Start a project's dev server in the background (logs go to the state folder).
    Start { project: String },
    /// Stop a dev server started by hangar.
    Stop { project: String },
    /// Print the last lines of a dev server's log.
    Logs {
        project: String,
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: usize,
    },
    /// Show or create the config file.
    Config {
        /// Write a commented starter config if none exists.
        #[arg(long)]
        init: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (mut cfg, warning) = Config::load(&config::config_path());
    if let Some(w) = &warning {
        eprintln!("warning: {w}");
    }
    if !cli.roots.is_empty() {
        cfg.roots = cli.roots.iter().map(|r| util::expand_tilde(r)).collect();
    }
    match run(cli.command, cfg) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Prints rows as aligned columns (the last column is never padded).
fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        let last = cells.len().saturating_sub(1);
        let text: Vec<String> = cells
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last {
                    c
                } else {
                    format!("{c:<w$}", w = widths[i])
                }
            })
            .collect();
        println!("{}", text.join("  ").trim_end());
    };
    line(headers.iter().map(|h| h.to_string()).collect());
    for row in rows {
        line(row.clone());
    }
}

fn run(cmd: Option<Cmd>, cfg: Config) -> anyhow::Result<ExitCode> {
    match cmd {
        None => {
            use std::io::IsTerminal;
            if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "the dashboard needs an interactive terminal; try `hangar projects`, `hangar ports` or `hangar deploys`"
                );
            }
            hangar::app::run(cfg)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Projects { json }) => {
            let list = projects::scan(&cfg.roots, cfg.max_depth, &cfg.ignore);
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(ExitCode::SUCCESS);
            }
            let rows: Vec<Vec<String>> = list
                .iter()
                .map(|p| {
                    let (branch, changes, last) = match &p.git {
                        Some(g) => (
                            g.branch.clone().unwrap_or_else(|| "(detached)".into()),
                            if g.dirty() {
                                format!(
                                    "{} changed",
                                    g.staged + g.modified + g.untracked + g.conflicted
                                )
                            } else {
                                "clean".into()
                            },
                            g.commits
                                .first()
                                .map(|c| util::ago(c.time))
                                .unwrap_or_else(|| "-".into()),
                        ),
                        None => (
                            "-".into(),
                            if p.git_error.is_some() {
                                "git error".into()
                            } else {
                                "-".into()
                            },
                            "-".into(),
                        ),
                    };
                    vec![
                        p.name.clone(),
                        p.kind.label().into(),
                        branch,
                        changes,
                        last,
                        util::tilde(&p.path),
                    ]
                })
                .collect();
            print_table(
                &["PROJECT", "KIND", "BRANCH", "CHANGES", "LAST", "PATH"],
                &rows,
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Ports { all, json }) => {
            let located = projects::locate(&cfg.roots, cfg.max_depth, &cfg.ignore);
            let mut scanner = ports::Scanner::new();
            let list: Vec<_> = scanner
                .scan(&located)?
                .into_iter()
                .filter(|l| all || cfg.show_all_ports || !l.system)
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(ExitCode::SUCCESS);
            }
            if list.is_empty() {
                println!("No dev servers listening. (Use --all to include system services.)");
                return Ok(ExitCode::SUCCESS);
            }
            let rows: Vec<Vec<String>> = list
                .iter()
                .map(|l| {
                    vec![
                        l.port.to_string(),
                        l.pid.to_string(),
                        l.process.clone(),
                        l.project.clone().unwrap_or_else(|| "-".into()),
                        l.started.map(util::ago).unwrap_or_else(|| "-".into()),
                        if l.memory > 0 {
                            util::bytes(l.memory)
                        } else {
                            "-".into()
                        },
                        util::truncate(&l.command, 70),
                    ]
                })
                .collect();
            print_table(
                &["PORT", "PID", "PROCESS", "PROJECT", "UP", "MEM", "COMMAND"],
                &rows,
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Deploys { json }) => {
            let list = projects::scan(&cfg.roots, cfg.max_depth, &cfg.ignore);
            let report = deploys::fetch(&cfg, &list);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(ExitCode::SUCCESS);
            }
            for (name, status) in [("vercel", &report.vercel), ("github", &report.github)] {
                if *status != deploys::SourceStatus::Ok {
                    eprintln!("{name}: {}", status.describe());
                }
            }
            let rows: Vec<Vec<String>> = report
                .deploys
                .iter()
                .map(|d| {
                    vec![
                        format!("{} {}", d.state.symbol(), d.state.label()),
                        d.project.clone(),
                        d.source.label().to_string(),
                        util::truncate(d.detail.as_deref().unwrap_or_default(), 22),
                        util::ago(d.created),
                        d.duration()
                            .map(util::duration)
                            .unwrap_or_else(|| "-".into()),
                        util::truncate(&d.title, 50),
                    ]
                })
                .collect();
            if rows.is_empty() {
                println!("No deploys or CI runs found.");
            } else {
                print_table(
                    &[
                        "STATE", "PROJECT", "SOURCE", "TARGET", "AGE", "TOOK", "TITLE",
                    ],
                    &rows,
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Kill { port, force }) => {
            let mut failed = false;
            for (l, res) in ports::kill_port(port, force)? {
                match res {
                    Ok(outcome) => {
                        failed |= outcome == ports::KillOutcome::StillRunning;
                        println!(
                            "{} (pid {}) on :{}: {}",
                            l.process,
                            l.pid,
                            port,
                            outcome.describe()
                        );
                    }
                    Err(e) => {
                        failed = true;
                        println!("{} (pid {}) on :{}: {e}", l.process, l.pid, port);
                    }
                }
            }
            Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Some(Cmd::Start { project }) => {
            let list = projects::scan(&cfg.roots, cfg.max_depth, &cfg.ignore);
            let p = projects::resolve(&list, &project).map_err(anyhow::Error::msg)?;
            let Some(cmd) = &p.dev_command else {
                anyhow::bail!("don't know how to start {} (no dev/start script)", p.name);
            };
            let run = Runner::default().start(&p.name, &p.path, cmd)?;
            println!(
                "started `{}` for {} (pid {})\nlogs: {}",
                cmd.join(" "),
                p.name,
                run.pid,
                util::tilde(&run.log)
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Stop { project }) => {
            let r = Runner::default();
            let running = r.list();
            let name = running
                .iter()
                .find(|x| x.project.eq_ignore_ascii_case(&project))
                .map(|x| x.project.clone())
                .unwrap_or(project);
            r.stop(&name)?;
            println!("stopped {name}");
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Logs { project, lines }) => {
            let r = Runner::default();
            let log = r
                .list()
                .into_iter()
                .find(|x| x.project.eq_ignore_ascii_case(&project))
                .map(|x| x.log)
                .unwrap_or_else(|| r.log_path(&project));
            if !log.exists() {
                anyhow::bail!("no log for {project} at {}", util::tilde(&log));
            }
            for line in runner::tail(&log, lines) {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Config { init }) => {
            let path = config::config_path();
            if init {
                if path.exists() {
                    println!("{} already exists", path.display());
                } else {
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&path, cfg.starter_toml())?;
                    println!("wrote {}", path.display());
                }
            } else {
                println!("{}", path.display());
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
