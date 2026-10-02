//! Git state via the `git` CLI (porcelain formats only, so output is stable across versions).

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::util::{self, git};

const GIT_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct GitInfo {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub commits: Vec<Commit>,
    pub remote: Option<Remote>,
}

impl GitInfo {
    pub fn dirty(&self) -> bool {
        self.staged + self.modified + self.untracked + self.conflicted > 0
    }

    pub fn last_commit_time(&self) -> Option<i64> {
        self.commits.first().map(|c| c.time)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub time: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Remote {
    pub url: String,
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl Remote {
    pub fn web_url(&self) -> String {
        format!("https://{}/{}/{}", self.host, self.owner, self.repo)
    }

    pub fn is_github(&self) -> bool {
        self.host.eq_ignore_ascii_case("github.com")
    }
}

/// Reads branch, sync state, working-tree counts, recent commits and the `origin` remote.
pub fn inspect(dir: &Path) -> Result<GitInfo> {
    let status = util::run(
        git(dir).args(["status", "--porcelain=v2", "--branch"]),
        GIT_TIMEOUT,
    )?;
    if !status.success() {
        bail!("git status: {}", status.error_line());
    }
    let mut info = parse_status(&status.stdout);

    // An empty repository has no log; that's not an error.
    let log = util::run(
        git(dir).args([
            "log",
            "-n",
            "5",
            "--no-color",
            "--format=%h%x1f%s%x1f%an%x1f%ct",
        ]),
        GIT_TIMEOUT,
    )?;
    if log.success() {
        info.commits = parse_log(&log.stdout);
    }

    let remote = util::run(git(dir).args(["remote", "get-url", "origin"]), GIT_TIMEOUT)?;
    if remote.success() {
        info.remote = parse_remote(remote.stdout.trim());
    }
    Ok(info)
}

/// Parses `git status --porcelain=v2 --branch`.
pub fn parse_status(text: &str) -> GitInfo {
    let mut info = GitInfo::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            info.branch = (rest != "(detached)").then(|| rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.oid ") {
            info.head = (rest != "(initial)").then(|| rest.chars().take(7).collect());
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            info.upstream = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for part in rest.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let xy: Vec<char> = line[2..].chars().take(2).collect();
            if xy.first().is_some_and(|c| *c != '.') {
                info.staged += 1;
            }
            if xy.get(1).is_some_and(|c| *c != '.') {
                info.modified += 1;
            }
        } else if line.starts_with("u ") {
            info.conflicted += 1;
        } else if line.starts_with("? ") {
            info.untracked += 1;
        }
    }
    info
}

/// Parses `git log --format=%h%x1f%s%x1f%an%x1f%ct`.
pub fn parse_log(text: &str) -> Vec<Commit> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\u{1f}');
            let sha = f.next()?.trim().to_string();
            let subject = f.next()?.to_string();
            let author = f.next()?.to_string();
            let time = f.next()?.trim().parse().ok()?;
            (!sha.is_empty()).then_some(Commit {
                sha,
                subject,
                author,
                time,
            })
        })
        .collect()
}

/// Understands https, ssh-style (`git@host:owner/repo`) and `ssh://` remotes. Strips credentials.
pub fn parse_remote(url: &str) -> Option<Remote> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let (host, path) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("git://"))
    {
        let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?.to_string(), path.to_string())
    } else {
        // scp-like: git@github.com:owner/repo.git
        let rest = url.rsplit_once('@').map(|(_, r)| r).unwrap_or(url);
        let (host, path) = rest.split_once(':')?;
        (host.to_string(), path.to_string())
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.rsplit_once('/')?;
    let owner = owner.rsplit('/').next()?.to_string();
    if host.is_empty() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    let display_url = if url.starts_with("http") {
        format!("https://{host}/{owner}/{repo}")
    } else {
        url.to_string()
    };
    Some(Remote {
        url: display_url,
        host,
        owner,
        repo: repo.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_with_upstream_and_changes() {
        let text = "\
# branch.oid 1a2b3c4d5e6f7a8b9c0d
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
1 .M N... 100644 100644 100644 abc abc src/main.rs
1 M. N... 100644 100644 100644 abc abc src/lib.rs
1 MM N... 100644 100644 100644 abc abc README.md
2 R. N... 100644 100644 100644 abc abc R100 new.rs\told.rs
u UU N... 100644 100644 100644 100644 a b c conflict.rs
? notes.txt
? scratch/
! ignored.log
";
        let s = parse_status(text);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.head.as_deref(), Some("1a2b3c4"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert_eq!(s.staged, 3);
        assert_eq!(s.modified, 2);
        assert_eq!(s.conflicted, 1);
        assert_eq!(s.untracked, 2);
        assert!(s.dirty());
    }

    #[test]
    fn status_detached_and_initial() {
        let s = parse_status("# branch.oid (initial)\n# branch.head (detached)\n");
        assert_eq!(s.branch, None);
        assert_eq!(s.head, None);
        assert!(!s.dirty());
    }

    #[test]
    fn log_parsing_skips_garbage() {
        let text =
            "abc1234\u{1f}Fix: thing\u{1f}Harsh\u{1f}1700000000\nbroken line\n\u{1f}\u{1f}\u{1f}\n";
        let c = parse_log(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].subject, "Fix: thing");
        assert_eq!(c[0].time, 1_700_000_000);
    }

    #[test]
    fn remotes() {
        for url in [
            "https://github.com/hamin2006/hangar.git",
            "https://github.com/hamin2006/hangar",
            "https://user:token@github.com/hamin2006/hangar.git",
            "git@github.com:hamin2006/hangar.git",
            "ssh://git@github.com/hamin2006/hangar.git",
            "ssh://git@github.com:22/hamin2006/hangar",
        ] {
            let r = parse_remote(url).unwrap_or_else(|| panic!("failed: {url}"));
            assert_eq!(
                (r.host.as_str(), r.owner.as_str(), r.repo.as_str()),
                ("github.com", "hamin2006", "hangar"),
                "{url}"
            );
            assert!(r.is_github());
            assert_eq!(r.web_url(), "https://github.com/hamin2006/hangar");
            assert!(
                !r.url.contains("token"),
                "credentials must not leak: {}",
                r.url
            );
        }
        let gl = parse_remote("git@gitlab.com:group/sub/project.git").unwrap();
        assert_eq!((gl.owner.as_str(), gl.repo.as_str()), ("sub", "project"));
        assert!(!gl.is_github());
        assert!(parse_remote("").is_none());
        assert!(parse_remote("not a url").is_none());
    }
}
