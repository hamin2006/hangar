//! Vercel REST API: recent deployments per linked project.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use super::{Deploy, Source, State, agent};
use crate::config::Config;
use crate::util;

pub struct Target {
    pub project: String,
    pub project_id: String,
    pub team_id: Option<String>,
}

pub struct Client {
    token: Mutex<String>,
    from_cli: bool,
    agent: ureq::Agent,
}

/// Last time we asked the Vercel CLI to refresh its token (at most every 10 minutes).
static LAST_REFRESH: AtomicI64 = AtomicI64::new(0);

/// Where `vercel login` stores credentials (`~/Library/Application Support` on macOS, `~/.local/share` on Linux).
pub fn cli_auth_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("com.vercel.cli").join("auth.json"))
}

fn read_cli_token() -> Option<String> {
    let text = std::fs::read_to_string(cli_auth_path()?).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("token")?
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
}

impl Client {
    /// Token from config, then `$VERCEL_TOKEN`, then the Vercel CLI's login.
    pub fn discover(cfg: &Config) -> Option<Self> {
        let explicit = cfg
            .vercel_token
            .clone()
            .or_else(|| std::env::var("VERCEL_TOKEN").ok())
            .filter(|t| !t.trim().is_empty());
        let (token, from_cli) = match explicit {
            Some(t) => (t, false),
            None => (read_cli_token()?, true),
        };
        Some(Self {
            token: Mutex::new(token),
            from_cli,
            agent: agent(),
        })
    }

    pub fn deployments(&self, t: &Target) -> Result<Vec<Deploy>> {
        let mut url = format!(
            "https://api.vercel.com/v6/deployments?projectId={}&limit=6",
            t.project_id
        );
        if let Some(team) = &t.team_id {
            url.push_str(&format!("&teamId={team}"));
        }
        let (status, body) = self.get(&url)?;
        let (status, body) =
            if (status == 401 || status == 403) && self.from_cli && self.refresh_cli_token() {
                self.get(&url)?
            } else {
                (status, body)
            };
        match status {
            200 => Ok(parse(&serde_json::from_str(&body)?, &t.project)),
            401 | 403 if self.from_cli => bail!("Vercel login expired; run `vercel login`"),
            401 | 403 => bail!("Vercel token rejected ({status})"),
            404 => bail!("project not found (re-run `vercel link`?)"),
            429 => bail!("rate limited by Vercel"),
            s => bail!("Vercel API returned {s}"),
        }
    }

    fn get(&self, url: &str) -> Result<(u16, String)> {
        let token = self.token.lock().map(|t| t.clone()).unwrap_or_default();
        let mut resp = self
            .agent
            .get(url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_to_string()?;
        Ok((status, body))
    }

    /// The CLI's OAuth token expires; running any authenticated CLI command refreshes it on disk.
    fn refresh_cli_token(&self) -> bool {
        let now = util::now_unix();
        let last = LAST_REFRESH.load(Ordering::Relaxed);
        if now - last < 600 || util::which("vercel").is_none() {
            return self.reload_token();
        }
        LAST_REFRESH.store(now, Ordering::Relaxed);
        let _ = util::run(
            Command::new("vercel").args(["whoami", "--no-color"]),
            Duration::from_secs(30),
        );
        self.reload_token()
    }

    /// Picks up a token another process refreshed. Returns true if it changed.
    fn reload_token(&self) -> bool {
        let Some(fresh) = read_cli_token() else {
            return false;
        };
        let Ok(mut current) = self.token.lock() else {
            return false;
        };
        if *current == fresh {
            return false;
        }
        *current = fresh;
        true
    }
}

fn state(s: &str) -> State {
    match s {
        "READY" => State::Success,
        "ERROR" => State::Failed,
        "CANCELED" => State::Canceled,
        "BUILDING" | "INITIALIZING" => State::Running,
        "QUEUED" => State::Queued,
        _ => State::Unknown,
    }
}

fn ms(v: Option<&Value>) -> Option<i64> {
    v?.as_i64().filter(|n| *n > 0).map(|n| n / 1000)
}

/// Parses the `/v6/deployments` response.
pub fn parse(json: &Value, project: &str) -> Vec<Deploy> {
    let Some(list) = json.get("deployments").and_then(Value::as_array) else {
        return vec![];
    };
    list.iter()
        .filter_map(|d| {
            let id = d.get("uid")?.as_str()?.to_string();
            let st = d
                .get("state")
                .or_else(|| d.get("readyState"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let state = state(st);
            let meta = d.get("meta");
            let m = |k: &str| {
                meta.and_then(|m| m.get(k))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            };
            let title = m("githubCommitMessage")
                .or_else(|| m("gitlabCommitMessage"))
                .or_else(|| m("bitbucketCommitMessage"))
                .map(|s| s.lines().next().unwrap_or_default().to_string())
                .unwrap_or_else(|| "Deployed from CLI".into());
            let created = ms(d.get("created")).or_else(|| ms(d.get("createdAt")))?;
            let finished = if state.active() {
                None
            } else {
                ms(d.get("ready"))
            };
            let target = d
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("preview")
                .to_string();
            Some(Deploy {
                id,
                project: project.to_string(),
                source: Source::Vercel,
                state,
                title,
                detail: Some(target),
                branch: m("githubCommitRef").or_else(|| m("gitlabCommitRef")),
                sha: m("githubCommitSha")
                    .or_else(|| m("gitlabCommitSha"))
                    .map(|s| s.chars().take(7).collect()),
                created,
                finished,
                url: d
                    .get("url")
                    .and_then(Value::as_str)
                    .map(|u| format!("https://{u}")),
                inspect_url: d
                    .get("inspectorUrl")
                    .and_then(Value::as_str)
                    .map(String::from),
                actor: d
                    .get("creator")
                    .and_then(|c| c.get("username"))
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
    fn parses_deployments_fixture() {
        let json: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/vercel_deployments.json"))
                .unwrap();
        let list = parse(&json, "wayfarer");
        assert_eq!(list.len(), 3, "entries without a uid are skipped");

        let a = &list[0];
        assert_eq!(a.state, State::Success);
        assert_eq!(a.title, "Add bosses");
        assert_eq!(a.detail.as_deref(), Some("production"));
        assert_eq!(a.branch.as_deref(), Some("main"));
        assert_eq!(a.sha.as_deref(), Some("abc1234"));
        assert_eq!(a.url.as_deref(), Some("https://wayfarer-abc.vercel.app"));
        assert_eq!(a.created, 1_759_300_000);
        assert_eq!(a.duration(), Some(42));
        assert_eq!(a.actor.as_deref(), Some("hamin2006"));

        assert_eq!(list[1].state, State::Running);
        assert_eq!(list[1].finished, None);
        assert_eq!(list[1].title, "Deployed from CLI");
        assert_eq!(list[1].detail.as_deref(), Some("preview"));

        assert_eq!(list[2].state, State::Failed);
        assert!(parse(&serde_json::json!({"error": {"code": "forbidden"}}), "x").is_empty());
    }
}
