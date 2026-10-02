//! Starts project dev servers in the background and keeps track of them.
//!
//! Each server runs in its own process group with output going to a log file, so it keeps running
//! after hangar exits, and stopping it also stops its children (`npm run dev` → `node vite`).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{config, ports, util};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub project: String,
    pub path: PathBuf,
    /// Also the process-group id.
    pub pid: u32,
    pub command: Vec<String>,
    pub log: PathBuf,
    pub started: i64,
}

pub struct Runner {
    dir: PathBuf,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new(config::state_dir())
    }
}

/// Process groups: `kill(-pgid, 0)` succeeds while any member is alive.
#[cfg(unix)]
pub fn group_alive(pgid: u32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: plain syscall with integer arguments.
    let rc = unsafe { libc::kill(-(pgid as i32), 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn group_alive(_pgid: u32) -> bool {
    false
}

fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "project".into() } else { s }
}

impl Runner {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn runs_file(&self) -> PathBuf {
        self.dir.join("runs.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn log_path(&self, project: &str) -> PathBuf {
        self.logs_dir().join(format!("{}.log", slug(project)))
    }

    fn read_all(&self) -> Vec<Run> {
        fs::read_to_string(self.runs_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Atomic write so a crash mid-save can't corrupt the file.
    fn save(&self, runs: &[Run]) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let tmp = self
            .dir
            .join(format!("runs.json.{}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec_pretty(runs)?)?;
        fs::rename(&tmp, self.runs_file())?;
        Ok(())
    }

    /// Servers still running. Records of dead servers are pruned.
    pub fn list(&self) -> Vec<Run> {
        let all = self.read_all();
        let alive: Vec<Run> = all.iter().filter(|r| group_alive(r.pid)).cloned().collect();
        if alive.len() != all.len() {
            let _ = self.save(&alive);
        }
        alive
    }

    pub fn find(&self, project: &str) -> Option<Run> {
        self.list().into_iter().find(|r| r.project == project)
    }

    pub fn start(&self, project: &str, path: &Path, command: &[String]) -> Result<Run> {
        let Some((program, args)) = command.split_first() else {
            bail!("no dev command for {project}")
        };
        if let Some(r) = self.find(project) {
            bail!("{project} is already running (pid {})", r.pid);
        }
        if !path.is_dir() {
            bail!("{} does not exist", path.display());
        }
        fs::create_dir_all(self.logs_dir())?;
        let log = self.log_path(project);
        if log.exists() {
            let _ = fs::rename(&log, log.with_extension("log.1"));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("cannot write {}", log.display()))?;
        writeln!(
            file,
            "── hangar: `{}` in {} ──",
            command.join(" "),
            util::tilde(path)
        )?;
        let err_file = file.try_clone()?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(path)
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(err_file)
            .env("NO_COLOR", "1")
            .env("FORCE_COLOR", "0")
            .env("BROWSER", "none");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("could not start `{program}` (is it installed?)"))?;
        let pid = child.id();

        // Catch servers that die immediately (missing deps, port in use...).
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(700) {
            if let Some(status) = child.try_wait()? {
                let tail = tail(&log, 4).join(" | ");
                bail!(
                    "exited immediately ({status}): {}",
                    if tail.is_empty() {
                        "no output".into()
                    } else {
                        tail
                    }
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Reap the child when it eventually exits so it never lingers as a zombie while hangar runs.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        let run = Run {
            project: project.to_string(),
            path: path.to_path_buf(),
            pid,
            command: command.to_vec(),
            log,
            started: util::now_unix(),
        };
        let mut runs = self.list();
        runs.retain(|r| r.project != project);
        runs.push(run.clone());
        self.save(&runs)?;
        Ok(run)
    }

    /// SIGTERM to the whole group, wait, and SIGKILL if `force` (or if it ignores us for 5 s).
    pub fn stop(&self, project: &str) -> Result<()> {
        let Some(run) = self.find(project) else {
            bail!("{project} isn't running")
        };
        ports::signal_group(run.pid, false)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while group_alive(run.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if group_alive(run.pid) {
            ports::signal_group(run.pid, true)?;
            std::thread::sleep(Duration::from_millis(300));
        }
        let runs: Vec<Run> = self
            .read_all()
            .into_iter()
            .filter(|r| r.project != project)
            .collect();
        self.save(&runs)?;
        if group_alive(run.pid) {
            bail!("{project} (pid {}) did not stop", run.pid);
        }
        Ok(())
    }
}

/// Last `max` lines of a log, with ANSI codes stripped. Reads at most the final 256 KB.
pub fn tail(path: &Path, max: usize) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return vec![];
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let window = 256 * 1024;
    if len > window {
        let _ = f.seek(SeekFrom::Start(len - window));
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return vec![];
    }
    let text = util::strip_ansi(&String::from_utf8_lossy(&buf));
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    if len > window && !lines.is_empty() {
        lines.remove(0); // probably cut mid-line
    }
    let skip = lines.len().saturating_sub(max);
    lines.split_off(skip)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn start_list_tail_stop() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let runner = Runner::new(state.path().to_path_buf());

        // A parent that spawns a child, like `npm run dev` → `vite`.
        let run = runner
            .start(
                "My App!",
                work.path(),
                &sh("echo \"\u{1b}[32mready\u{1b}[0m in $(pwd)\"; sleep 60 & wait"),
            )
            .unwrap();
        assert!(run.log.ends_with("logs/My-App.log"));
        assert!(group_alive(run.pid));
        assert_eq!(runner.list().len(), 1);
        assert!(
            runner
                .start("My App!", work.path(), &sh("sleep 1"))
                .unwrap_err()
                .to_string()
                .contains("already running")
        );

        std::thread::sleep(Duration::from_millis(300));
        let lines = tail(&run.log, 10);
        assert!(
            lines.iter().any(|l| l.starts_with("ready in ")),
            "{lines:?}"
        );
        assert!(lines.iter().all(|l| !l.contains('\u{1b}')));

        runner.stop("My App!").unwrap();
        assert!(
            !group_alive(run.pid),
            "the whole group, including the backgrounded child, is gone"
        );
        assert!(runner.list().is_empty());
        assert!(runner.stop("My App!").is_err());
    }

    #[test]
    fn immediate_failures_are_reported() {
        let state = tempfile::tempdir().unwrap();
        let runner = Runner::new(state.path().to_path_buf());
        let work = tempfile::tempdir().unwrap();
        let err = runner
            .start(
                "bad",
                work.path(),
                &sh("echo 'Error: port 3000 in use' >&2; exit 1"),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("exited immediately") && err.contains("port 3000 in use"),
            "{err}"
        );
        assert!(
            runner
                .start(
                    "missing",
                    work.path(),
                    &["definitely-not-a-binary-xyz".into()]
                )
                .is_err()
        );
        assert!(
            runner
                .start("nodir", &work.path().join("nope"), &sh("true"))
                .is_err()
        );
        assert!(runner.list().is_empty());
    }

    #[test]
    fn dead_records_are_pruned_and_logs_rotate() {
        let state = tempfile::tempdir().unwrap();
        let runner = Runner::new(state.path().to_path_buf());
        runner
            .save(&[Run {
                project: "ghost".into(),
                path: "/tmp".into(),
                pid: 999_999,
                command: vec![],
                log: "/x".into(),
                started: 0,
            }])
            .unwrap();
        assert!(runner.list().is_empty());
        assert!(runner.read_all().is_empty());

        let log = runner.log_path("x");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "old run\n").unwrap();
        let work = tempfile::tempdir().unwrap();
        runner.start("x", work.path(), &sh("sleep 30")).unwrap();
        assert_eq!(
            fs::read_to_string(log.with_extension("log.1")).unwrap(),
            "old run\n"
        );
        runner.stop("x").unwrap();
    }

    #[test]
    fn tail_handles_big_and_missing_files() {
        let d = tempfile::tempdir().unwrap();
        assert!(tail(&d.path().join("missing.log"), 5).is_empty());
        let big = d.path().join("big.log");
        let mut f = File::create(&big).unwrap();
        for i in 0..40_000 {
            writeln!(f, "line {i}").unwrap();
        }
        let t = tail(&big, 3);
        assert_eq!(t, ["line 39997", "line 39998", "line 39999"]);
        assert_eq!(slug("../etc/passwd"), "etc-passwd");
        assert_eq!(slug("***"), "project");
    }
}
