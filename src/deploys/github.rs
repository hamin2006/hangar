//! GitHub REST API: recent Actions workflow runs per repository.

use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use super::{Deploy, Source, State, agent};
use crate::config::Config;
use crate::util;

pub struct Target {
    pub project: String,
    pub owner: String,
    pub repo: String,
}

pub struct Client {
    token: String,
    agent: ureq::Agent,
}

impl Client {
    /// Token from config, then `$GITHUB_TOKEN` / `$GH_TOKEN`, then `gh auth token`.
    pub fn discover(cfg: &Config) -> Option<Self> {
        let token = cfg
            .github_token
            .clone()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok())
            .or_else(|| std::env::var("GH_TOKEN").ok())
            .filter(|t| !t.trim().is_empty())
            .or_else(gh_cli_token)?;
        Some(Self {
            token,
            agent: agent(),
        })
    }

    pub fn runs(&self, t: &Target) -> Result<Vec<Deploy>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/actions/runs?per_page=5",
            t.owner, t.repo
        );
        let mut resp = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()?;
        let status = resp.status().as_u16();
        let remaining = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u32>().ok());
        let body = resp
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_to_string()?;
        match status {
            200 => Ok(parse(&serde_json::from_str(&body)?, &t.project)),
            // No access, or Actions disabled: simply nothing to show for this repo.
            404 => Ok(vec![]),
            401 => bail!("GitHub token rejected; run `gh auth login`"),
            403 | 429 if remaining == Some(0) => bail!("GitHub API rate limit reached; will retry"),
            403 => bail!("GitHub denied access (token scopes?)"),
            s => bail!("GitHub API returned {s}"),
        }
    }
}

fn gh_cli_token() -> Option<String> {
    util::which("gh")?;
    let out = util::run(
        Command::new("gh").args(["auth", "token"]),
        Duration::from_secs(5),
    )
    .ok()?;
    let token = out.stdout.trim();
    (out.success() && !token.is_empty()).then(|| token.to_string())
}

fn state(status: &str, conclusion: Option<&str>) -> State {
    match status {
        "completed" => match conclusion {
            Some("success") | Some("neutral") => State::Success,
            Some("failure") | Some("timed_out") | Some("startup_failure") => State::Failed,
            Some("cancelled") | Some("skipped") | Some("stale") => State::Canceled,
            Some("action_required") => State::Queued,
            _ => State::Unknown,
        },
        "in_progress" => State::Running,
        "queued" | "waiting" | "requested" | "pending" => State::Queued,
        _ => State::Unknown,
    }
}

fn time(v: Option<&Value>) -> Option<i64> {
    let s = v?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp())
}

/// Parses the `/actions/runs` response.
pub fn parse(json: &Value, project: &str) -> Vec<Deploy> {
    let Some(runs) = json.get("workflow_runs").and_then(Value::as_array) else {
        return vec![];
    };
    runs.iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_u64()?.to_string();
            let status = r.get("status").and_then(Value::as_str).unwrap_or("");
            let state = state(status, r.get("conclusion").and_then(Value::as_str));
            let s = |k: &str| {
                r.get(k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            };
            let created = time(r.get("run_started_at")).or_else(|| time(r.get("created_at")))?;
            Some(Deploy {
                id,
                project: project.to_string(),
                source: Source::GitHub,
                state,
                title: s("display_title")
                    .or_else(|| s("name"))
                    .unwrap_or_else(|| "workflow run".into()),
                detail: s("name"),
                branch: s("head_branch"),
                sha: s("head_sha").map(|x| x.chars().take(7).collect()),
                created,
                finished: if status == "completed" && !state.active() {
                    time(r.get("updated_at"))
                } else {
                    None
                },
                url: s("html_url"),
                inspect_url: None,
                actor: r
                    .get("actor")
                    .and_then(|a| a.get("login"))
                    .and_then(Value::as_str)
                    .map(String::from),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runs_fixture() {
        let json: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/github_runs.json")).unwrap();
        let runs = parse(&json, "hangar");
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].state, State::Running);
        assert_eq!(runs[0].finished, None);
        assert_eq!(runs[0].detail.as_deref(), Some("CI"));
        assert_eq!(runs[0].title, "Add deploys tab");
        assert_eq!(runs[0].sha.as_deref(), Some("deadbee"));
        assert_eq!(runs[1].state, State::Failed);
        assert_eq!(runs[1].duration(), Some(95));
        assert_eq!(
            runs[1].url.as_deref(),
            Some("https://github.com/hamin2006/hangar/actions/runs/2")
        );
        assert_eq!(runs[2].state, State::Success);
        assert_eq!(runs[3].state, State::Canceled);
    }

    #[test]
    fn status_mapping() {
        assert_eq!(state("queued", None), State::Queued);
        assert_eq!(state("completed", Some("timed_out")), State::Failed);
        assert_eq!(state("completed", Some("skipped")), State::Canceled);
        assert_eq!(state("completed", None), State::Unknown);
        assert_eq!(state("mystery", None), State::Unknown);
    }
}
