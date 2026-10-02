//! Figures out what kind of project a folder is and how to start its dev server.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Vite,
    Next,
    Node,
    Rust,
    Python,
    Go,
    Static,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Vite => "vite",
            Kind::Next => "next",
            Kind::Node => "node",
            Kind::Rust => "rust",
            Kind::Python => "python",
            Kind::Go => "go",
            Kind::Static => "static",
            Kind::Other => "-",
        }
    }
}

/// Files whose presence makes a folder a project.
pub const MARKERS: &[&str] = &[
    ".git",
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "deno.json",
];

pub fn is_project(dir: &Path) -> bool {
    MARKERS.iter().any(|m| dir.join(m).exists())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Detected {
    pub kind: Kind,
    /// Program and arguments for the dev server, e.g. `["npm", "run", "dev"]`.
    pub dev_command: Option<Vec<String>>,
    /// npm-style scripts, for display.
    pub scripts: Vec<String>,
}

pub fn detect(dir: &Path) -> Detected {
    if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
        return detect_node(dir, &text);
    }
    if dir.join("Cargo.toml").exists() {
        return Detected {
            kind: Kind::Rust,
            dev_command: Some(vec!["cargo".into(), "run".into()]),
            scripts: vec![],
        };
    }
    if dir.join("go.mod").exists() {
        return Detected {
            kind: Kind::Go,
            dev_command: Some(vec!["go".into(), "run".into(), ".".into()]),
            scripts: vec![],
        };
    }
    if dir.join("pyproject.toml").exists() || dir.join("requirements.txt").exists() {
        let dev = dir
            .join("manage.py")
            .exists()
            .then(|| vec!["python3".into(), "manage.py".into(), "runserver".into()]);
        return Detected {
            kind: Kind::Python,
            dev_command: dev,
            scripts: vec![],
        };
    }
    if dir.join("index.html").exists() {
        return Detected {
            kind: Kind::Static,
            dev_command: None,
            scripts: vec![],
        };
    }
    Detected {
        kind: Kind::Other,
        dev_command: None,
        scripts: vec![],
    }
}

fn detect_node(dir: &Path, package_json: &str) -> Detected {
    let pkg: serde_json::Value = serde_json::from_str(package_json).unwrap_or_default();
    let has_dep = |name: &str| {
        ["dependencies", "devDependencies"]
            .iter()
            .any(|k| pkg.get(k).and_then(|d| d.get(name)).is_some())
    };
    let kind = if has_dep("next") {
        Kind::Next
    } else if has_dep("vite") {
        Kind::Vite
    } else {
        Kind::Node
    };
    let scripts: Vec<String> = pkg
        .get("scripts")
        .and_then(|s| s.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let manager = package_manager(dir);
    let dev_command = ["dev", "start", "serve"]
        .iter()
        .find(|s| scripts.iter().any(|k| k == *s))
        .map(|script| vec![manager.to_string(), "run".into(), script.to_string()]);
    Detected {
        kind,
        dev_command,
        scripts,
    }
}

fn package_manager(dir: &Path) -> &'static str {
    if dir.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if dir.join("yarn.lock").exists() {
        "yarn"
    } else if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
        "bun"
    } else {
        "npm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn node_kinds_and_dev_scripts() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("package.json"),
            r#"{"scripts":{"dev":"vite","build":"vite build"},"devDependencies":{"vite":"^8"}}"#,
        )
        .unwrap();
        let det = detect(d.path());
        assert_eq!(det.kind, Kind::Vite);
        assert_eq!(det.dev_command.unwrap(), ["npm", "run", "dev"]);

        fs::write(d.path().join("pnpm-lock.yaml"), "").unwrap();
        fs::write(
            d.path().join("package.json"),
            r#"{"scripts":{"start":"next start"},"dependencies":{"next":"16"}}"#,
        )
        .unwrap();
        let det = detect(d.path());
        assert_eq!(det.kind, Kind::Next);
        assert_eq!(det.dev_command.unwrap(), ["pnpm", "run", "start"]);
    }

    #[test]
    fn broken_package_json_is_still_a_node_project() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("package.json"), "{ not json").unwrap();
        let det = detect(d.path());
        assert_eq!(det.kind, Kind::Node);
        assert_eq!(det.dev_command, None);
    }

    #[test]
    fn other_languages() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect(d.path()).kind, Kind::Rust);

        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("go.mod"), "module x").unwrap();
        assert_eq!(detect(d.path()).kind, Kind::Go);

        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("requirements.txt"), "").unwrap();
        fs::write(d.path().join("manage.py"), "").unwrap();
        let det = detect(d.path());
        assert_eq!(det.kind, Kind::Python);
        assert_eq!(det.dev_command.unwrap()[1], "manage.py");

        let d = tempfile::tempdir().unwrap();
        assert!(!is_project(d.path()));
        assert_eq!(detect(d.path()).kind, Kind::Other);
    }
}
