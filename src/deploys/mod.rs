//! Deployments (Vercel) and CI runs (GitHub Actions) in one model.

pub mod github;
pub mod vercel;

use std::time::Duration;

use serde::Serialize;

use crate::config::Config;
use crate::projects::Project;
use crate::util;

pub const HTTP_TIMEOUT: Duration = Duration::from_secs(12);
/// Repos queried per refresh, most recently active first, to stay well inside API rate limits.
const MAX_GITHUB_REPOS: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Queued,
    Failed,
    Success,
    Canceled,
    Unknown,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Running => "building",
            State::Queued => "queued",
            State::Failed => "failed",
            State::Success => "ready",
            State::Canceled => "canceled",
            State::Unknown => "unknown",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            State::Running => "◐",
            State::Queued => "○",
            State::Failed => "✗",
            State::Success => "✓",
            State::Canceled => "⊘",
            State::Unknown => "?",
        }
    }

    pub fn active(self) -> bool {
        matches!(self, State::Running | State::Queued)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Vercel,
    #[serde(rename = "github")]
    GitHub,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Vercel => "▲ vercel",
            Source::GitHub => "⚙ actions",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Deploy {
    pub id: String,
    pub project: String,
    pub source: Source,
    pub state: State,
    /// Commit message (Vercel) or run title (GitHub).
    pub title: String,
    /// `production`/`preview` (Vercel) or the workflow name (GitHub).
    pub detail: Option<String>,
    pub branch: Option<String>,
    pub sha: Option<String>,
    pub created: i64,
    pub finished: Option<i64>,
    /// The page to open: the deployment itself, or the run page.
    pub url: Option<String>,
    /// Vercel's build inspector.
    pub inspect_url: Option<String>,
    pub actor: Option<String>,
}

impl Deploy {
    pub fn in_flight(&self) -> bool {
        self.state == State::Running
            || (self.state == State::Queued && util::now_unix() - self.created < 1800)
    }

    /// Build/run time. Only running deploys count up live; queued ones have no duration yet. Values over
    /// a day are dropped because GitHub's `updated_at` moves when old runs are touched (e.g. log expiry).
    pub fn duration(&self) -> Option<i64> {
        let end = self
            .finished
            .or_else(|| (self.state == State::Running).then(util::now_unix))?;
        Some(end - self.created).filter(|d| (0..=86_400).contains(d))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", content = "message", rename_all = "snake_case")]
pub enum SourceStatus {
    /// Nothing to query (no linked projects) or no credentials.
    Off(String),
    Ok,
    Error(String),
}

impl SourceStatus {
    pub fn describe(&self) -> String {
        match self {
            SourceStatus::Off(m) => m.clone(),
            SourceStatus::Ok => "ok".into(),
            SourceStatus::Error(e) => e.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    pub deploys: Vec<Deploy>,
    pub vercel: SourceStatus,
    pub github: SourceStatus,
    pub fetched_at: i64,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            deploys: vec![],
            vercel: SourceStatus::Off("not loaded yet".into()),
            github: SourceStatus::Off("not loaded yet".into()),
            fetched_at: 0,
        }
    }
}

impl Report {
    /// Newest deploy for a project (any source), for the Projects tab.
    pub fn latest_for(&self, project: &str) -> Option<&Deploy> {
        self.deploys
            .iter()
            .filter(|d| d.project == project)
            .max_by_key(|d| d.created)
    }

    /// Deploys genuinely in flight: running, or queued within the last 30 minutes (older "queued"
    /// entries are usually runs waiting on a manual approval).
    pub fn active_count(&self) -> usize {
        self.deploys.iter().filter(|d| d.in_flight()).count()
    }

    /// Failed runs that are still the latest for their project+source (i.e. not yet fixed).
    pub fn failing(&self) -> Vec<&Deploy> {
        let mut out = vec![];
        for d in &self.deploys {
            let newest = self
                .deploys
                .iter()
                .filter(|o| o.project == d.project && o.source == d.source && o.detail == d.detail)
                .max_by_key(|o| o.created);
            if d.state == State::Failed && newest.is_some_and(|n| n.id == d.id) {
                out.push(d);
            }
        }
        out
    }
}

/// Collects one error per source, keeping the first message and a count of the rest.
#[derive(Default)]
struct Errors {
    first: Option<String>,
    count: usize,
    ok: usize,
}

impl Errors {
    fn status(self, off: Option<String>) -> SourceStatus {
        if let Some(reason) = off {
            return SourceStatus::Off(reason);
        }
        match self.first {
            Some(e) if self.ok == 0 => SourceStatus::Error(e),
            Some(e) => SourceStatus::Error(format!("{e} (+{} more)", self.count.saturating_sub(1))),
            None => SourceStatus::Ok,
        }
    }
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .http_status_as_error(false)
        .user_agent(concat!("hangar/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Fetches deploys for every linked project. Never fails as a whole: per-source problems are reported
/// in the returned statuses so the UI can show them next to whatever did load.
pub fn fetch(cfg: &Config, projects: &[Project]) -> Report {
    let mut by_activity: Vec<&Project> = projects.iter().collect();
    by_activity.sort_by_key(|p| std::cmp::Reverse(p.activity()));

    // Several clones of one repo (or links to one Vercel project) must only be fetched once, under the
    // most recently active clone's name.
    let mut seen = std::collections::HashSet::new();
    let vercel_targets: Vec<vercel::Target> = by_activity
        .iter()
        .filter_map(|p| {
            let link = p
                .vercel
                .as_ref()
                .filter(|l| seen.insert(l.project_id.clone()))?;
            Some(vercel::Target {
                project: p.name.clone(),
                project_id: link.project_id.clone(),
                team_id: link.team_id().map(String::from),
            })
        })
        .collect();
    let github_targets: Vec<github::Target> = by_activity
        .iter()
        .filter_map(|p| {
            let r = p.remote().filter(|r| {
                r.is_github() && seen.insert(format!("{}/{}", r.owner, r.repo).to_ascii_lowercase())
            })?;
            Some(github::Target {
                project: p.name.clone(),
                owner: r.owner.clone(),
                repo: r.repo.clone(),
            })
        })
        .take(MAX_GITHUB_REPOS)
        .collect();

    let (vercel_res, github_res) = std::thread::scope(|s| {
        let v = s.spawn(|| fetch_vercel(cfg, &vercel_targets));
        let g = s.spawn(|| fetch_github(cfg, &github_targets));
        (v.join(), g.join())
    });
    let (mut deploys, vercel) =
        vercel_res.unwrap_or_else(|_| (vec![], SourceStatus::Error("internal error".into())));
    let (gh_deploys, github) =
        github_res.unwrap_or_else(|_| (vec![], SourceStatus::Error("internal error".into())));
    deploys.extend(gh_deploys);
    deploys.sort_by_key(|d| std::cmp::Reverse(d.created));
    Report {
        deploys,
        vercel,
        github,
        fetched_at: util::now_unix(),
    }
}

fn fetch_vercel(cfg: &Config, targets: &[vercel::Target]) -> (Vec<Deploy>, SourceStatus) {
    if targets.is_empty() {
        return (
            vec![],
            SourceStatus::Off("no projects linked with `vercel link`".into()),
        );
    }
    let Some(client) = vercel::Client::discover(cfg) else {
        return (
            vec![],
            SourceStatus::Off("not logged in (run `vercel login` or set VERCEL_TOKEN)".into()),
        );
    };
    let mut errors = Errors::default();
    let mut out = vec![];
    for t in targets {
        match client.deployments(t) {
            Ok(list) => {
                errors.ok += 1;
                out.extend(list);
            }
            Err(e) => {
                errors.count += 1;
                errors
                    .first
                    .get_or_insert_with(|| format!("{}: {e}", t.project));
            }
        }
    }
    (out, errors.status(None))
}

fn fetch_github(cfg: &Config, targets: &[github::Target]) -> (Vec<Deploy>, SourceStatus) {
    if targets.is_empty() {
        return (
            vec![],
            SourceStatus::Off("no projects with a GitHub remote".into()),
        );
    }
    let Some(client) = github::Client::discover(cfg) else {
        return (
            vec![],
            SourceStatus::Off("not logged in (run `gh auth login` or set GITHUB_TOKEN)".into()),
        );
    };
    let results: Vec<(String, anyhow::Result<Vec<Deploy>>)> = std::thread::scope(|s| {
        let handles: Vec<_> = targets
            .chunks(targets.len().div_ceil(6).max(1))
            .map(|batch| {
                let client = &client;
                s.spawn(move || {
                    batch
                        .iter()
                        .map(|t| (t.project.clone(), client.runs(t)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    let mut errors = Errors::default();
    let mut out = vec![];
    for (project, res) in results {
        match res {
            Ok(list) => {
                errors.ok += 1;
                out.extend(list);
            }
            Err(e) => {
                errors.count += 1;
                errors
                    .first
                    .get_or_insert_with(|| format!("{project}: {e}"));
            }
        }
    }
    (out, errors.status(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, project: &str, state: State, created: i64) -> Deploy {
        Deploy {
            id: id.into(),
            project: project.into(),
            source: Source::Vercel,
            state,
            title: String::new(),
            detail: Some("production".into()),
            branch: None,
            sha: None,
            created,
            finished: Some(created + 30),
            url: None,
            inspect_url: None,
            actor: None,
        }
    }

    #[test]
    fn failing_only_counts_unfixed_failures() {
        let r = Report {
            deploys: vec![
                d("1", "a", State::Failed, 100),
                d("2", "a", State::Success, 200),
                d("3", "b", State::Failed, 150),
            ],
            ..Report::default()
        };
        let failing: Vec<_> = r.failing().into_iter().map(|d| d.id.as_str()).collect();
        assert_eq!(failing, ["3"]);
        assert_eq!(r.latest_for("a").unwrap().id, "2");
        assert_eq!(r.active_count(), 0);
        assert_eq!(d("x", "a", State::Success, 10).duration(), Some(30));
        let queued = Deploy {
            finished: None,
            ..d("q", "a", State::Queued, 10)
        };
        assert_eq!(queued.duration(), None);
        let stale = Deploy {
            finished: Some(10 + 90 * 86_400),
            ..d("s", "a", State::Success, 10)
        };
        assert_eq!(stale.duration(), None);
    }

    #[test]
    fn error_summaries() {
        assert_eq!(Errors::default().status(None), SourceStatus::Ok);
        assert_eq!(
            Errors::default().status(Some("off".into())),
            SourceStatus::Off("off".into())
        );
        let e = Errors {
            first: Some("x: 401".into()),
            count: 3,
            ok: 1,
        };
        assert_eq!(
            e.status(None),
            SourceStatus::Error("x: 401 (+2 more)".into())
        );
        let e = Errors {
            first: Some("x: 401".into()),
            count: 3,
            ok: 0,
        };
        assert_eq!(e.status(None), SourceStatus::Error("x: 401".into()));
    }
}
