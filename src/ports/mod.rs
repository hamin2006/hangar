//! Listening TCP ports, who owns them, and killing them safely.

pub mod parse;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::util;
use parse::RawSocket;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Listener {
    pub port: u16,
    pub addrs: Vec<String>,
    pub pid: u32,
    pub process: String,
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub memory: u64,
    pub cpu: f32,
    pub started: Option<i64>,
    /// Owned by the current user.
    pub mine: bool,
    /// Name of the project whose folder the process runs in.
    pub project: Option<String>,
    /// System daemons, desktop-app helpers and other users' processes: hidden unless "show all" is on.
    pub system: bool,
}

impl Listener {
    pub fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Only reachable from this machine.
    pub fn local_only(&self) -> bool {
        self.addrs
            .iter()
            .all(|a| a == "127.0.0.1" || a == "::1" || a == "localhost")
    }
}

/// Lists raw listening sockets using the best available source for this OS.
pub fn raw_sockets() -> Result<Vec<RawSocket>> {
    #[cfg(target_os = "linux")]
    {
        let procfs = procfs_sockets();
        match procfs {
            Ok(s) if !s.is_empty() => return Ok(s),
            _ if util::which("lsof").is_none() => return procfs,
            _ => {}
        }
    }
    lsof_sockets()
}

fn lsof_sockets() -> Result<Vec<RawSocket>> {
    if util::which("lsof").is_none() {
        bail!("`lsof` is not installed");
    }
    let out = util::run(
        Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"]),
        Duration::from_secs(8),
    )?;
    // lsof exits 1 when nothing matches (an empty list, not an error) and prints warnings about
    // filesystems it can't stat; only a failure with no output at all is a real error.
    let stderr = out.stderr.trim();
    if !out.success()
        && out.stdout.trim().is_empty()
        && !stderr.is_empty()
        && !stderr.contains("WARNING")
    {
        bail!("lsof: {}", out.error_line());
    }
    Ok(parse::lsof(&out.stdout))
}

#[cfg(target_os = "linux")]
fn procfs_sockets() -> Result<Vec<RawSocket>> {
    use std::collections::HashMap;
    let mut sockets = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(file) {
            sockets.extend(parse::proc_net_tcp(&text));
        }
    }
    if sockets.is_empty() {
        return Ok(vec![]);
    }
    // Map socket inodes to the processes holding them (only our own processes are readable).
    let mut owners: HashMap<u64, (u32, String)> = HashMap::new();
    for entry in std::fs::read_dir("/proc")?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let mut comm: Option<String> = None;
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path())
                && let Some(inode) = parse::socket_inode(&link.to_string_lossy())
            {
                let name = comm.get_or_insert_with(|| {
                    std::fs::read_to_string(entry.path().join("comm"))
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                });
                owners.insert(inode, (pid, name.clone()));
            }
        }
    }
    Ok(sockets
        .into_iter()
        .map(|s| {
            let (pid, process) = owners.get(&s.inode).cloned().unwrap_or((0, "?".into()));
            RawSocket {
                pid,
                process,
                addr: s.addr,
                port: s.port,
            }
        })
        .collect())
}

/// Merges IPv4/IPv6 duplicates into one row per (port, pid), sorted by port.
pub fn group(raw: Vec<RawSocket>) -> Vec<Listener> {
    let mut map: BTreeMap<(u16, u32), Listener> = BTreeMap::new();
    for s in raw {
        let entry = map.entry((s.port, s.pid)).or_insert_with(|| Listener {
            port: s.port,
            addrs: vec![],
            pid: s.pid,
            process: s.process.clone(),
            command: s.process.clone(),
            cwd: None,
            memory: 0,
            cpu: 0.0,
            started: None,
            mine: false,
            project: None,
            system: false,
        });
        if !entry.addrs.contains(&s.addr) {
            entry.addrs.push(s.addr);
        }
    }
    let mut list: Vec<Listener> = map.into_values().collect();
    for l in &mut list {
        l.addrs.sort();
        if l.addrs.iter().any(|a| a == "*") {
            l.addrs = vec!["*".into()];
        }
    }
    list
}

const SYSTEM_PREFIXES: &[&str] = &[
    "/System/",
    "/usr/libexec/",
    "/usr/sbin/",
    "/sbin/",
    "/usr/lib/systemd/",
    "/lib/systemd/",
];

/// OS daemons and desktop-app helpers (Spotify, VS Code, OneDrive…) are noise on a dev dashboard.
/// Docker Desktop is the exception: its port forwards are usually your containers.
pub fn is_background(exe: &str) -> bool {
    let lower = exe.to_ascii_lowercase();
    if lower.contains("docker") {
        return false;
    }
    SYSTEM_PREFIXES.iter().any(|pre| exe.starts_with(pre)) || exe.contains(".app/Contents/")
}

/// Keeps a `sysinfo::System` alive between scans so CPU usage can be measured.
pub struct Scanner {
    sys: System,
    me: Option<sysinfo::Uid>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        let mut sys = System::new();
        let self_pid = Pid::from_u32(std::process::id());
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self_pid]),
            true,
            ProcessRefreshKind::nothing().with_user(UpdateKind::Always),
        );
        let me = sys.process(self_pid).and_then(|p| p.user_id().cloned());
        Self { sys, me }
    }

    /// Scans listeners and attributes them to projects. `projects` are (name, folder) pairs.
    pub fn scan(&mut self, projects: &[(String, PathBuf)]) -> Result<Vec<Listener>> {
        let mut list = group(raw_sockets()?);
        let pids: Vec<Pid> = list
            .iter()
            .filter(|l| l.pid > 0)
            .map(|l| Pid::from_u32(l.pid))
            .collect();
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing()
                .with_memory()
                .with_cpu()
                .with_cmd(UpdateKind::Always)
                .with_cwd(UpdateKind::Always)
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_user(UpdateKind::Always),
        );
        for l in &mut list {
            let Some(p) = self.sys.process(Pid::from_u32(l.pid)) else {
                l.system = true;
                continue;
            };
            let cmd: Vec<String> = p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            if !cmd.is_empty() {
                l.command = cmd.join(" ");
            }
            l.cwd = p
                .cwd()
                .map(Path::to_path_buf)
                .filter(|c| c.as_os_str() != "/");
            l.memory = p.memory();
            l.cpu = p.cpu_usage();
            l.started = Some(p.start_time() as i64).filter(|t| *t > 0);
            l.mine = self.me.is_some() && p.user_id() == self.me.as_ref();
            let exe = p
                .exe()
                .map(|e| e.to_string_lossy().into_owned())
                .or_else(|| cmd.first().cloned())
                .unwrap_or_default();
            l.system = !l.mine || is_background(&exe);
            l.project = l.cwd.as_deref().and_then(|cwd| project_for(projects, cwd));
            if l.project.is_some() {
                l.system = false;
            }
        }
        Ok(list)
    }
}

/// Deepest project folder containing `path`, comparing canonical paths so `/var` vs `/private/var`
/// and symlinked roots still match.
pub fn project_for(projects: &[(String, PathBuf)], path: &Path) -> Option<String> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    projects
        .iter()
        .filter(|(_, dir)| {
            path.starts_with(dir)
                || canon.starts_with(dir.canonicalize().unwrap_or_else(|_| dir.clone()))
        })
        .max_by_key(|(_, dir)| dir.components().count())
        .map(|(name, _)| name.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum KillOutcome {
    /// Exited after SIGTERM.
    Terminated,
    /// Exited after SIGKILL.
    Killed,
    /// Still alive after SIGTERM and the grace period (try force).
    StillRunning,
    /// Already gone.
    NotFound,
}

impl KillOutcome {
    pub fn describe(self) -> &'static str {
        match self {
            KillOutcome::Terminated => "stopped",
            KillOutcome::Killed => "force-killed",
            KillOutcome::StillRunning => "still running (use force kill)",
            KillOutcome::NotFound => "already gone",
        }
    }
}

/// Refuses to touch init, kernel threads or hangar itself.
pub fn check_killable(pid: u32) -> Result<()> {
    if pid <= 1 {
        bail!("refusing to signal pid {pid}");
    }
    if pid == std::process::id() {
        bail!("refusing to kill hangar itself");
    }
    Ok(())
}

#[cfg(unix)]
fn signal(pid: i32, sig: i32) -> Result<bool> {
    // SAFETY: kill(2) has no memory-safety preconditions; we only pass plain integers.
    let rc = unsafe { libc::kill(pid, sig) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => bail!("permission denied (the process belongs to another user)"),
        _ => bail!("kill failed: {err}"),
    }
}

/// True while the process exists and isn't a zombie (a process that exited but whose parent hasn't
/// collected it yet: it no longer holds ports or does anything).
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    match signal(pid as i32, 0) {
        Ok(true) => !is_zombie(pid),
        Ok(false) => false,
        Err(_) => true, // exists but owned by someone else
    }
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    // The state letter follows the parenthesised command name, which may itself contain ") ".
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| rest.starts_with('Z')))
        .unwrap_or(false)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn is_zombie(pid: u32) -> bool {
    util::run(
        Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]),
        Duration::from_secs(2),
    )
    .map(|o| o.stdout.trim_start().starts_with('Z'))
    .unwrap_or(false)
}

fn wait_gone(pid: u32, grace: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < grace {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    !alive(pid)
}

/// SIGTERM, wait `grace`; if `force`, follow up with SIGKILL.
#[cfg(unix)]
pub fn kill_pid(pid: u32, force: bool, grace: Duration) -> Result<KillOutcome> {
    check_killable(pid)?;
    if !signal(pid as i32, libc::SIGTERM)? {
        return Ok(KillOutcome::NotFound);
    }
    if wait_gone(pid, grace) {
        return Ok(KillOutcome::Terminated);
    }
    if !force {
        return Ok(KillOutcome::StillRunning);
    }
    if !signal(pid as i32, libc::SIGKILL)? {
        return Ok(KillOutcome::Terminated);
    }
    Ok(if wait_gone(pid, Duration::from_secs(2)) {
        KillOutcome::Killed
    } else {
        KillOutcome::StillRunning
    })
}

/// Signals a whole process group (used for dev servers we started).
#[cfg(unix)]
pub fn signal_group(pgid: u32, force: bool) -> Result<bool> {
    check_killable(pgid)?;
    signal(
        -(pgid as i32),
        if force { libc::SIGKILL } else { libc::SIGTERM },
    )
}

#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    false
}

#[cfg(not(unix))]
pub fn kill_pid(_pid: u32, _force: bool, _grace: Duration) -> Result<KillOutcome> {
    bail!("killing processes is only supported on macOS and Linux")
}

#[cfg(not(unix))]
pub fn signal_group(_pgid: u32, _force: bool) -> Result<bool> {
    bail!("only supported on macOS and Linux")
}

/// Kills every process listening on `port`.
pub fn kill_port(port: u16, force: bool) -> Result<Vec<(Listener, Result<KillOutcome>)>> {
    let targets: Vec<Listener> = group(raw_sockets()?)
        .into_iter()
        .filter(|l| l.port == port)
        .collect();
    if targets.is_empty() {
        bail!("nothing is listening on port {port}");
    }
    Ok(targets
        .into_iter()
        .map(|l| {
            let res = kill_pid(l.pid, force, Duration::from_secs(3));
            (l, res)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pid: u32, addr: &str, port: u16) -> RawSocket {
        RawSocket {
            pid,
            process: "node".into(),
            addr: addr.into(),
            port,
        }
    }

    #[test]
    fn groups_dual_stack_sockets() {
        let list = group(vec![
            raw(10, "127.0.0.1", 5173),
            raw(10, "::1", 5173),
            raw(11, "*", 3000),
            raw(11, "::1", 3000),
            raw(10, "::1", 5173),
        ]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].port, 3000);
        assert_eq!(list[0].addrs, ["*"]);
        assert!(!list[0].local_only());
        assert_eq!(list[1].addrs, ["127.0.0.1", "::1"]);
        assert!(list[1].local_only());
        assert_eq!(list[1].url(), "http://localhost:5173");
    }

    #[test]
    fn project_attribution_prefers_deepest() {
        let d = tempfile::tempdir().unwrap();
        let outer = d.path().join("mono");
        let inner = outer.join("apps/web");
        std::fs::create_dir_all(&inner).unwrap();
        let projects = vec![
            ("mono".to_string(), outer.clone()),
            ("web".to_string(), inner.clone()),
        ];
        assert_eq!(
            project_for(&projects, &inner.join("src")).as_deref(),
            Some("web")
        );
        assert_eq!(project_for(&projects, &outer).as_deref(), Some("mono"));
        assert_eq!(project_for(&projects, d.path()), None);
    }

    #[test]
    fn background_heuristic() {
        assert!(is_background(
            "/System/Library/CoreServices/ControlCenter.app/Contents/MacOS/ControlCenter"
        ));
        assert!(is_background(
            "/Applications/Spotify.app/Contents/MacOS/Spotify"
        ));
        assert!(is_background("/usr/sbin/sshd"));
        assert!(!is_background(
            "/Applications/Docker.app/Contents/MacOS/com.docker.backend"
        ));
        assert!(!is_background("/opt/homebrew/bin/postgres"));
        assert!(!is_background("/Users/me/.nvm/versions/node/v24/bin/node"));
    }

    #[test]
    fn refuses_dangerous_targets() {
        assert!(check_killable(0).is_err());
        assert!(check_killable(1).is_err());
        assert!(check_killable(std::process::id()).is_err());
        assert!(check_killable(424_242).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn kills_a_child_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        // Reap in the background so the child doesn't linger as a zombie.
        let reaper = std::thread::spawn(move || child.wait());
        assert!(alive(pid));
        assert_eq!(
            kill_pid(pid, false, Duration::from_secs(3)).unwrap(),
            KillOutcome::Terminated
        );
        reaper.join().unwrap().unwrap();
        assert!(!alive(pid));
        assert_eq!(
            kill_pid(pid, false, Duration::from_millis(100)).unwrap(),
            KillOutcome::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn escalates_when_sigterm_is_ignored() {
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(200));
        let reaper = std::thread::spawn(move || child.wait());
        assert_eq!(
            kill_pid(pid, false, Duration::from_millis(400)).unwrap(),
            KillOutcome::StillRunning
        );
        assert_eq!(
            kill_pid(pid, true, Duration::from_millis(400)).unwrap(),
            KillOutcome::Killed
        );
        reaper.join().unwrap().unwrap();
    }
}
