use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hangar::config::{self, Config};
use hangar::{projects, util};

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

fn run(cmd: Option<Cmd>, cfg: Config) -> anyhow::Result<ExitCode> {
    match cmd {
        None => {
            eprintln!("the dashboard isn't built yet; try `hangar projects`");
            Ok(ExitCode::FAILURE)
        }
        Some(Cmd::Projects { json }) => {
            let list = projects::scan(&cfg.roots, cfg.max_depth, &cfg.ignore);
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for p in &list {
                    let branch = p
                        .git
                        .as_ref()
                        .and_then(|g| g.branch.clone())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{:<24} {:<7} {:<16} {}",
                        p.name,
                        p.kind.label(),
                        branch,
                        util::tilde(&p.path)
                    );
                }
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
