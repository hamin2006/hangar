//! Optional user configuration at `~/.config/hangar/config.toml`. Every field has a safe default, and a
//! broken file never stops hangar from starting: it falls back to defaults and reports a warning.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::expand_tilde;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Folders scanned for projects.
    pub roots: Vec<PathBuf>,
    /// How many folder levels below each root to look for projects.
    pub max_depth: usize,
    /// Folder names never descended into.
    pub ignore: Vec<String>,
    /// Command used to open a project. Defaults to $VISUAL, $EDITOR, then `code`.
    pub editor: Option<String>,
    /// Show listeners owned by other users / system services on the Ports tab.
    pub show_all_ports: bool,
    pub refresh: Refresh,
    /// Tokens are read from the environment or the Vercel/GitHub CLIs when unset here.
    pub vercel_token: Option<String>,
    pub github_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Refresh {
    pub ports_secs: u64,
    pub projects_secs: u64,
    pub deploys_secs: u64,
}

impl Default for Refresh {
    fn default() -> Self {
        Self {
            ports_secs: 2,
            projects_secs: 15,
            deploys_secs: 30,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let roots = [
            "Desktop/Projects",
            "Projects",
            "projects",
            "code",
            "dev",
            "src",
            "repos",
            "Developer",
        ]
        .iter()
        .map(|r| home.join(r))
        .filter(|p| p.is_dir())
        .collect();
        Self {
            roots,
            max_depth: 2,
            ignore: [
                "node_modules",
                "target",
                "dist",
                "build",
                ".next",
                ".venv",
                "venv",
                "vendor",
                "__pycache__",
                "Library",
                "Applications",
            ]
            .map(String::from)
            .to_vec(),
            editor: None,
            show_all_ports: false,
            refresh: Refresh::default(),
            vercel_token: None,
            github_token: None,
        }
    }
}

impl Config {
    /// Parses config text. Paths starting with `~` are expanded.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.roots = cfg.roots.iter().map(|p| expand_tilde(p)).collect();
        cfg.refresh.ports_secs = cfg.refresh.ports_secs.max(1);
        cfg.refresh.projects_secs = cfg.refresh.projects_secs.max(2);
        cfg.refresh.deploys_secs = cfg.refresh.deploys_secs.max(10);
        cfg.max_depth = cfg.max_depth.clamp(0, 6);
        Ok(cfg)
    }

    /// Loads the config file if present. Returns defaults plus a warning if it can't be read or parsed.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match Self::parse(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (
                    Self::default(),
                    Some(format!(
                        "config ignored ({}): {}",
                        path.display(),
                        first_line(&e)
                    )),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(e) => (
                Self::default(),
                Some(format!("could not read {}: {e}", path.display())),
            ),
        }
    }

    pub fn editor_command(&self) -> String {
        self.editor
            .clone()
            .or_else(|| std::env::var("VISUAL").ok())
            .or_else(|| std::env::var("EDITOR").ok())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "code".into())
    }

    /// A commented starter file written by `hangar config --init`.
    pub fn starter_toml(&self) -> String {
        let roots = self
            .roots
            .iter()
            .map(|r| format!("  \"{}\",", crate::util::tilde(r)))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            r#"# hangar configuration. Every setting is optional.

# Folders scanned for projects (a project is a folder with .git, package.json, Cargo.toml, ...).
roots = [
{roots}
]

# How many levels below each root to look for projects.
max_depth = {depth}

# Folder names that are never scanned.
ignore = {ignore:?}

# Command used to open a project (defaults to $VISUAL, $EDITOR, then `code`).
# editor = "code"

# Also show ports owned by other users and system services.
show_all_ports = false

[refresh]
ports_secs = 2
projects_secs = 15
deploys_secs = 30

# Tokens default to $VERCEL_TOKEN / $GITHUB_TOKEN, then the `vercel` and `gh` CLI logins.
# vercel_token = "..."
# github_token = "..."
"#,
            depth = self.max_depth,
            ignore = self.ignore,
        )
    }
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s).trim()
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(fallback)
        })
}

/// `$HANGAR_CONFIG`, else `$XDG_CONFIG_HOME/hangar/config.toml`, else `~/.config/hangar/config.toml`.
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HANGAR_CONFIG") {
        return PathBuf::from(p);
    }
    xdg("XDG_CONFIG_HOME", ".config")
        .join("hangar")
        .join("config.toml")
}

/// Where logs and dev-server records live. `$HANGAR_STATE_DIR` overrides it (used by tests).
pub fn state_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("HANGAR_STATE_DIR") {
        return PathBuf::from(p);
    }
    xdg("XDG_STATE_HOME", ".local/state").join("hangar")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_defaults() {
        let cfg = Config::parse("roots = [\"~/work\"]\n[refresh]\nports_secs = 0\n").unwrap();
        assert_eq!(cfg.roots, vec![dirs::home_dir().unwrap().join("work")]);
        assert_eq!(cfg.refresh.ports_secs, 1, "clamped to a sane minimum");
        assert_eq!(cfg.refresh.deploys_secs, 30);
        assert_eq!(cfg.max_depth, 2);
        assert!(cfg.ignore.contains(&"node_modules".to_string()));
    }

    #[test]
    fn bad_config_falls_back_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "roots = 5\n").unwrap();
        let (cfg, warn) = Config::load(&path);
        assert_eq!(cfg, Config::default());
        assert!(warn.unwrap().contains("config ignored"));

        std::fs::write(&path, "typo_field = true\n").unwrap();
        assert!(
            Config::load(&path).1.is_some(),
            "unknown keys are reported, not silently ignored"
        );

        let (_, warn) = Config::load(&dir.path().join("missing.toml"));
        assert!(warn.is_none());
    }

    #[test]
    fn starter_file_parses() {
        let cfg = Config::default();
        let parsed = Config::parse(&cfg.starter_toml()).unwrap();
        assert_eq!(parsed.max_depth, cfg.max_depth);
        assert_eq!(parsed.ignore, cfg.ignore);
    }
}
