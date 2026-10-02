//! Background refresh threads. Each owns one data source, refreshes on a timer or on request, and
//! sends results to the UI over a channel. Dropping [`Workers`] stops them.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::deploys::{self, Report};
use crate::ports::{Listener, Scanner};
use crate::projects::{self, Project};
use crate::runner::{Run, Runner};

#[derive(Debug)]
pub enum Event {
    Projects(Vec<Project>),
    Ports(Result<Vec<Listener>, String>),
    Deploys(Report),
    Runs(Vec<Run>),
    /// A user action finished. `refresh` asks for an immediate reload of the affected data.
    ActionDone {
        ok: bool,
        message: String,
        refresh: Refresh,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Refresh {
    pub projects: bool,
    pub ports: bool,
    pub deploys: bool,
}

impl Refresh {
    pub const PORTS: Refresh = Refresh {
        projects: false,
        ports: true,
        deploys: false,
    };
    pub const DEPLOYS: Refresh = Refresh {
        projects: false,
        ports: false,
        deploys: true,
    };
    pub const ALL: Refresh = Refresh {
        projects: true,
        ports: true,
        deploys: true,
    };
}

pub struct Workers {
    projects: Sender<()>,
    ports: Sender<()>,
    deploys: Sender<()>,
}

impl Workers {
    pub fn request(&self, r: Refresh) {
        if r.projects {
            let _ = self.projects.send(());
        }
        if r.ports {
            let _ = self.ports.send(());
        }
        if r.deploys {
            let _ = self.deploys.send(());
        }
    }
}

/// Waits for the next tick. Returns false when the UI has gone away. Extra queued requests collapse.
fn wait(rx: &Receiver<()>, every: Duration) -> bool {
    match rx.recv_timeout(every) {
        Ok(()) => {
            while rx.try_recv().is_ok() {}
            true
        }
        Err(RecvTimeoutError::Timeout) => true,
        Err(RecvTimeoutError::Disconnected) => false,
    }
}

/// Runs `f`, converting a panic into an error message so one bad refresh can't kill the worker.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "worker panicked".into())
    })
}

pub fn spawn(cfg: Arc<Config>, events: Sender<Event>, runner: Arc<Runner>) -> Workers {
    let shared: Arc<Mutex<Option<Vec<Project>>>> = Arc::new(Mutex::new(None));
    let (projects_tx, projects_rx) = mpsc::channel();
    let (ports_tx, ports_rx) = mpsc::channel();
    let (deploys_tx, deploys_rx) = mpsc::channel();

    // Projects: full git scan.
    {
        let (cfg, events, shared, deploys_kick) = (
            cfg.clone(),
            events.clone(),
            shared.clone(),
            deploys_tx.clone(),
        );
        thread::Builder::new()
            .name("hangar-projects".into())
            .spawn(move || {
                let mut first = true;
                loop {
                    let list = guarded(|| projects::scan(&cfg.roots, cfg.max_depth, &cfg.ignore))
                        .unwrap_or_default();
                    if let Ok(mut s) = shared.lock() {
                        *s = Some(list.clone());
                    }
                    if events.send(Event::Projects(list)).is_err() {
                        return;
                    }
                    if first {
                        first = false;
                        let _ = deploys_kick.send(());
                    }
                    if !wait(&projects_rx, Duration::from_secs(cfg.refresh.projects_secs)) {
                        return;
                    }
                }
            })
            .expect("spawn projects worker");
    }

    // Ports + dev servers: cheap, so frequent. Attribution uses a quick folder listing until the full
    // project scan lands.
    {
        let (cfg, events, shared, runner) =
            (cfg.clone(), events.clone(), shared.clone(), runner.clone());
        thread::Builder::new()
            .name("hangar-ports".into())
            .spawn(move || {
                let mut scanner = Scanner::new();
                let mut quick: Option<Vec<(String, std::path::PathBuf)>> = None;
                loop {
                    let located: Vec<(String, std::path::PathBuf)> =
                        match shared.lock().ok().and_then(|s| s.clone()) {
                            Some(list) => list
                                .iter()
                                .map(|p| (p.name.clone(), p.path.clone()))
                                .collect(),
                            None => quick
                                .get_or_insert_with(|| {
                                    projects::locate(&cfg.roots, cfg.max_depth, &cfg.ignore)
                                })
                                .clone(),
                        };
                    let ports = guarded(|| scanner.scan(&located).map_err(|e| e.to_string()))
                        .and_then(|r| r);
                    let runs = guarded(|| runner.list()).unwrap_or_default();
                    if events.send(Event::Ports(ports)).is_err()
                        || events.send(Event::Runs(runs)).is_err()
                    {
                        return;
                    }
                    if !wait(&ports_rx, Duration::from_secs(cfg.refresh.ports_secs)) {
                        return;
                    }
                }
            })
            .expect("spawn ports worker");
    }

    // Deploys: network, so slow; polls faster while something is building.
    {
        let (cfg, events, shared) = (cfg.clone(), events.clone(), shared.clone());
        thread::Builder::new()
            .name("hangar-deploys".into())
            .spawn(move || {
                // Wait for the first project scan before the first fetch.
                loop {
                    if shared.lock().map(|s| s.is_some()).unwrap_or(true) {
                        break;
                    }
                    if !wait(&deploys_rx, Duration::from_millis(500)) {
                        return;
                    }
                }
                loop {
                    let projects = shared
                        .lock()
                        .ok()
                        .and_then(|s| s.clone())
                        .unwrap_or_default();
                    let report =
                        guarded(|| deploys::fetch(&cfg, &projects)).unwrap_or_else(|e| Report {
                            vercel: deploys::SourceStatus::Error(e.clone()),
                            github: deploys::SourceStatus::Error(e),
                            ..Report::default()
                        });
                    let busy = report.active_count() > 0;
                    if events.send(Event::Deploys(report)).is_err() {
                        return;
                    }
                    let every = if busy {
                        Duration::from_secs(8)
                    } else {
                        Duration::from_secs(cfg.refresh.deploys_secs)
                    };
                    if !wait(&deploys_rx, every) {
                        return;
                    }
                }
            })
            .expect("spawn deploys worker");
    }

    Workers {
        projects: projects_tx,
        ports: ports_tx,
        deploys: deploys_tx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarded_turns_panics_into_errors() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let r: Result<(), String> = guarded(|| panic!("boom"));
        std::panic::set_hook(prev);
        assert_eq!(r.unwrap_err(), "boom");
        assert_eq!(guarded(|| 5).unwrap(), 5);
    }

    #[test]
    fn wait_collapses_requests_and_detects_shutdown() {
        let (tx, rx) = mpsc::channel();
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        assert!(wait(&rx, Duration::from_secs(5)));
        assert!(rx.try_recv().is_err(), "queued duplicates were drained");
        drop(tx);
        assert!(!wait(&rx, Duration::from_secs(5)));
    }
}
