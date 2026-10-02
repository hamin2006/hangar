//! Finds projects under the configured roots and gathers their state.

pub mod detect;
pub mod git;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use detect::Kind;
pub use git::{Commit, GitInfo, Remote};

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub dev_command: Option<Vec<String>>,
    pub scripts: Vec<String>,
    pub git: Option<GitInfo>,
    /// Set when the folder has a `.git` but git couldn't be read (e.g. `git` missing, corrupt repo).
    pub git_error: Option<String>,
    pub vercel: Option<VercelLink>,
}

impl Project {
    /// Most recent activity, for sorting: last commit time, else folder modification time.
    pub fn activity(&self) -> i64 {
        self.git
            .as_ref()
            .and_then(GitInfo::last_commit_time)
            .or_else(|| {
                std::fs::metadata(&self.path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
            })
            .unwrap_or(0)
    }

    pub fn remote(&self) -> Option<&Remote> {
        self.git.as_ref()?.remote.as_ref()
    }
}

/// Contents of `.vercel/project.json`, written by `vercel link`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VercelLink {
    pub project_id: String,
    pub org_id: String,
    #[serde(default)]
    pub project_name: Option<String>,
}

impl VercelLink {
    pub fn read(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join(".vercel").join("project.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Team-owned projects need `teamId` on API calls; personal ones must not send it.
    pub fn team_id(&self) -> Option<&str> {
        self.org_id
            .starts_with("team_")
            .then_some(self.org_id.as_str())
    }
}

/// Lists project folders (without inspecting them). A project's subfolders are not searched further,
/// hidden folders and ignored names are skipped, and symlink loops can't recurse forever.
pub fn find(roots: &[PathBuf], max_depth: usize, ignore: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        walk(root, 0, max_depth, ignore, &mut seen, &mut out);
    }
    out
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    ignore: &[String],
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    let Ok(canonical) = dir.canonicalize() else {
        return;
    };
    if !seen.insert(canonical) {
        return;
    }
    if detect::is_project(dir) {
        out.push(dir.to_path_buf());
        return;
    }
    if depth >= max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type()
                .map(|t| t.is_dir() || t.is_symlink())
                .unwrap_or(false)
        })
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !name.starts_with('.') && !ignore.contains(&name)
        })
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    children.sort();
    for child in children {
        walk(&child, depth + 1, max_depth, ignore, seen, out);
    }
}

/// Builds full project info for one folder. Never fails: problems are recorded on the project.
pub fn inspect(path: &Path) -> Project {
    let det = detect::detect(path);
    let (git, git_error) = if path.join(".git").exists() {
        match git::inspect(path) {
            Ok(info) => (Some(info), None),
            Err(e) => (None, Some(e.to_string())),
        }
    } else {
        (None, None)
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    Project {
        name,
        path: path.to_path_buf(),
        kind: det.kind,
        dev_command: det.dev_command,
        scripts: det.scripts,
        git,
        git_error,
        vercel: VercelLink::read(path),
    }
}

/// Finds and inspects every project in parallel, most recently active first.
pub fn scan(roots: &[PathBuf], max_depth: usize, ignore: &[String]) -> Vec<Project> {
    let paths = find(roots, max_depth, ignore);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8);
    let chunk = paths.len().div_ceil(workers).max(1);
    let mut projects: Vec<Project> = std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|batch| s.spawn(move || batch.iter().map(|p| inspect(p)).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    disambiguate_names(&mut projects);
    projects.sort_by_key(|p| std::cmp::Reverse(p.activity()));
    projects
}

/// Two projects with the same folder name (in different roots) get their parent folder appended.
fn disambiguate_names(projects: &mut [Project]) {
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for p in projects.iter() {
        *counts.entry(p.name.clone()).or_default() += 1;
    }
    for p in projects.iter_mut() {
        if counts[&p.name] > 1
            && let Some(parent) = p.path.parent().and_then(|x| x.file_name())
        {
            p.name = format!("{} ({})", p.name, parent.to_string_lossy());
        }
    }
}

/// The project whose folder contains `path` (deepest match wins).
pub fn owner_of<'a>(projects: &'a [Project], path: &Path) -> Option<&'a Project> {
    projects
        .iter()
        .filter(|p| path.starts_with(&p.path))
        .max_by_key(|p| p.path.components().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_projects_respecting_depth_ignore_and_nesting() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        for p in [
            "a",
            "group/b",
            "group/deep/c",
            "node_modules/pkg",
            ".hidden/x",
            "a/sub",
        ] {
            fs::create_dir_all(r.join(p)).unwrap();
        }
        fs::write(r.join("a/package.json"), "{}").unwrap();
        fs::write(r.join("a/sub/Cargo.toml"), "").unwrap(); // inside a project: not listed
        fs::write(r.join("group/b/Cargo.toml"), "").unwrap();
        fs::write(r.join("group/deep/c/go.mod"), "").unwrap(); // depth 3: too deep
        fs::write(r.join("node_modules/pkg/package.json"), "{}").unwrap();
        fs::write(r.join(".hidden/x/package.json"), "{}").unwrap();

        let found = find(&[r.to_path_buf()], 2, &["node_modules".to_string()]);
        let names: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(r).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["a", "group/b"]);

        let deeper = find(&[r.to_path_buf()], 3, &["node_modules".to_string()]);
        assert_eq!(deeper.len(), 3);

        assert!(find(&[r.join("does-not-exist")], 2, &[]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_terminate() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir_all(r.join("x/y")).unwrap();
        std::os::unix::fs::symlink(r, r.join("x/y/loop")).unwrap();
        assert!(find(&[r.to_path_buf()], 6, &[]).is_empty());
    }

    #[test]
    fn duplicate_names_are_disambiguated() {
        let root = tempfile::tempdir().unwrap();
        for p in ["one/app", "two/app"] {
            fs::create_dir_all(root.path().join(p)).unwrap();
            fs::write(root.path().join(p).join("package.json"), "{}").unwrap();
        }
        let mut names: Vec<_> = scan(&[root.path().to_path_buf()], 2, &[])
            .into_iter()
            .map(|p| p.name)
            .collect();
        names.sort();
        assert_eq!(names, ["app (one)", "app (two)"]);
    }

    #[test]
    fn vercel_link_and_owner_lookup() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("site");
        fs::create_dir_all(dir.join(".vercel")).unwrap();
        fs::write(dir.join("package.json"), "{}").unwrap();
        fs::write(
            dir.join(".vercel/project.json"),
            r#"{"projectId":"prj_1","orgId":"team_9","projectName":"site"}"#,
        )
        .unwrap();
        let p = inspect(&dir);
        let link = p.vercel.clone().unwrap();
        assert_eq!(link.team_id(), Some("team_9"));
        assert_eq!(
            VercelLink {
                org_id: "abc".into(),
                ..link
            }
            .team_id(),
            None
        );

        let projects = vec![p];
        assert!(owner_of(&projects, &dir.join("src/deep")).is_some());
        assert!(owner_of(&projects, root.path()).is_none());
    }
}
