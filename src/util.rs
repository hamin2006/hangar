//! Small helpers shared by every module: commands with hard timeouts and human-friendly formatting.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use wait_timeout::ChildExt;

/// Captured result of a finished command.
#[derive(Debug, Clone)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The most useful single line to show a user when the command failed.
    pub fn error_line(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        text.lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or("command failed")
            .to_string()
    }
}

/// Runs `cmd` with no stdin and a hard timeout, capturing stdout/stderr.
///
/// Output pipes are drained on background threads so a chatty process can't deadlock, and the reader
/// threads are abandoned (not joined) if a grandchild keeps the pipe open past the deadline.
pub fn run(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run `{program}`"))?;

    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    for (is_err, pipe) in [
        (
            false,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
        (
            true,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
    ] {
        if let Some(mut pipe) = pipe {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send((is_err, buf));
            });
        }
    }
    drop(tx);

    let start = Instant::now();
    let status = match child.wait_timeout(timeout)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{program}` timed out after {}s", timeout.as_secs());
        }
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for _ in 0..2 {
        let left = timeout
            .saturating_sub(start.elapsed())
            .max(Duration::from_millis(200));
        match rx.recv_timeout(left) {
            Ok((true, buf)) => stderr = buf,
            Ok((false, buf)) => stdout = buf,
            Err(_) => break,
        }
    }
    Ok(Output {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// A `git` command that can never prompt for credentials or take repository locks.
pub fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
    c
}

/// True if `program` can be found on PATH.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compact relative age: `now`, `42s`, `5m`, `3h`, `2d`, `6w`, `4mo`, `2y`.
pub fn age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=4 => "now".into(),
        5..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        86_400..=1_209_599 => format!("{}d", s / 86_400),
        1_209_600..=5_183_999 => format!("{}w", s / 604_800),
        5_184_000..=31_535_999 => format!("{}mo", s / 2_592_000),
        _ => format!("{}y", s / 31_536_000),
    }
}

/// Age of a unix timestamp relative to now.
pub fn ago(ts: i64) -> String {
    age(now_unix() - ts)
}

/// `45s`, `3m 05s`, `2h 14m`.
pub fn duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// `512B`, `12K`, `340M`, `1.2G`.
pub fn bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n}B")
    } else if f < K * K {
        format!("{:.0}K", f / K)
    } else if f < K * K * K {
        format!("{:.0}M", f / (K * K))
    } else {
        format!("{:.1}G", f / (K * K * K))
    }
}

/// Removes ANSI escape sequences (colors, cursor moves) and stray carriage returns from log text.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: parameters then a final byte in @..~
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: terminated by BEL or ESC \
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\r' => {}
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// Shortens a path for display by replacing the home directory with `~`.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// Expands a leading `~` in user-supplied paths.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Truncates to `max` display characters, adding an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Runs a command in the background without waiting (used for "open in editor/browser/finder").
pub fn spawn_detached(cmd: &mut Command) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("could not launch `{program}`"))?;
    Ok(())
}

/// Copies text to the system clipboard using the platform's CLI tool.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    let candidates: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (prog, args) in candidates {
        if which(prog).is_none() {
            continue;
        }
        let mut child = Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .context("no stdin")?
            .write_all(text.as_bytes())?;
        match child.wait_timeout(Duration::from_secs(3))? {
            Some(s) if s.success() => return Ok(()),
            Some(_) => bail!("{prog} failed"),
            None => {
                let _ = child.kill();
                bail!("{prog} timed out");
            }
        }
    }
    bail!("no clipboard tool found (pbcopy, wl-copy, xclip or xsel)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(age(-3), "now");
        assert_eq!(age(3), "now");
        assert_eq!(age(42), "42s");
        assert_eq!(age(61), "1m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(86_400 * 3), "3d");
        assert_eq!(age(86_400 * 20), "2w");
        assert_eq!(age(86_400 * 90), "3mo");
        assert_eq!(age(86_400 * 800), "2y");
    }

    #[test]
    fn durations_and_sizes() {
        assert_eq!(duration(9), "9s");
        assert_eq!(duration(185), "3m 05s");
        assert_eq!(duration(8040), "2h 14m");
        assert_eq!(bytes(900), "900B");
        assert_eq!(bytes(2048), "2K");
        assert_eq!(bytes(350 * 1024 * 1024), "350M");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 / 2), "1.5G");
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(
            strip_ansi("\u{1b}[32m  VITE\u{1b}[39m ready\r\n"),
            "  VITE ready\n"
        );
        assert_eq!(
            strip_ansi("a\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}b"),
            "alinkb"
        );
        assert_eq!(strip_ansi("\u{1b}[2K\u{1b}[1Gdone"), "done");
        assert_eq!(strip_ansi("tab\tok"), "tab\tok");
    }

    #[test]
    fn truncates() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("héllo", 3), "hé…");
        assert_eq!(truncate("x", 0), "");
    }

    #[test]
    fn run_captures_and_times_out() {
        let out = run(
            Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "out");
        assert_eq!(out.stderr.trim(), "err");
        assert_eq!(out.code, Some(3));
        assert_eq!(out.error_line(), "err");

        let start = Instant::now();
        let err = run(Command::new("sleep").arg("10"), Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(3));

        assert!(
            run(
                &mut Command::new("definitely-not-a-real-program-xyz"),
                Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn tilde_round_trip() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(tilde(&home.join("code/app")), "~/code/app");
        assert_eq!(expand_tilde(Path::new("~/code")), home.join("code"));
        assert_eq!(expand_tilde(Path::new("/tmp/x")), PathBuf::from("/tmp/x"));
    }
}
