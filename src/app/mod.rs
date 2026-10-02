//! The interactive dashboard: state, key handling and actions. Rendering lives in [`ui`].

pub mod ui;
pub mod worker;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};

use crate::config::Config;
use crate::deploys::{Deploy, Report};
use crate::ports::{self, Listener};
use crate::projects::Project;
use crate::runner::{self, Run, Runner};
use crate::util;
use worker::{Event, Refresh, Workers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Projects,
    Ports,
    Deploys,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Projects, Tab::Ports, Tab::Deploys];

    pub fn index(self) -> usize {
        match self {
            Tab::Projects => 0,
            Tab::Ports => 1,
            Tab::Deploys => 2,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Projects => "Projects",
            Tab::Ports => "Ports",
            Tab::Deploys => "Deploys",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Kill {
        pid: u32,
        port: u16,
        process: String,
        force: bool,
    },
    StopServer {
        project: String,
    },
    Deploy {
        project: String,
        path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Popup {
    Help,
    Confirm {
        title: String,
        body: String,
        action: Action,
    },
    Logs {
        title: String,
        path: PathBuf,
        lines: Vec<String>,
        scroll: usize,
        follow: bool,
        read_at: Option<Instant>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Err,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub text: String,
    pub level: Level,
    pub at: Instant,
}

/// Something the main loop must do outside the app (it owns the terminal).
#[derive(Debug)]
pub enum Effect {
    /// Suspend the dashboard and run an interactive program (e.g. a terminal editor).
    Foreground(Command),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Loaded {
    pub projects: bool,
    pub ports: bool,
    pub deploys: bool,
}

pub struct App {
    pub cfg: Arc<Config>,
    pub tab: Tab,
    pub projects: Vec<Project>,
    pub ports: Vec<Listener>,
    pub ports_error: Option<String>,
    pub deploys: Report,
    pub runs: Vec<Run>,
    pub loaded: Loaded,
    pub updated: [Option<Instant>; 3],
    pub selected: [usize; 3],
    selected_key: [Option<String>; 3],
    pub filters: [String; 3],
    pub editing_filter: bool,
    pub show_all_ports: bool,
    pub popup: Option<Popup>,
    pub status: Option<Status>,
    pub busy: Vec<String>,
    pub quit: bool,
    events: Sender<Event>,
    workers: Option<Workers>,
    runner: Arc<Runner>,
    matcher: SkimMatcherV2,
}

const LOG_LINES: usize = 2000;

impl App {
    pub fn new(cfg: Arc<Config>, events: Sender<Event>, runner: Arc<Runner>) -> Self {
        let show_all_ports = cfg.show_all_ports;
        Self {
            cfg,
            tab: Tab::Projects,
            projects: vec![],
            ports: vec![],
            ports_error: None,
            deploys: Report::default(),
            runs: vec![],
            loaded: Loaded::default(),
            updated: [None; 3],
            selected: [0; 3],
            selected_key: [None, None, None],
            filters: Default::default(),
            editing_filter: false,
            show_all_ports,
            popup: None,
            status: None,
            busy: vec![],
            quit: false,
            events,
            workers: None,
            runner,
            matcher: SkimMatcherV2::default().ignore_case(),
        }
    }

    pub fn attach_workers(&mut self, w: Workers) {
        self.workers = Some(w);
    }

    fn refresh(&self, r: Refresh) {
        if let Some(w) = &self.workers {
            w.request(r);
        }
    }

    pub fn set_status(&mut self, level: Level, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            level,
            at: Instant::now(),
        });
    }

    // ---------- Visible (filtered) rows ----------

    fn filter_rank<'a, T>(
        &self,
        items: impl Iterator<Item = &'a T>,
        text: impl Fn(&T) -> String,
        filter: &str,
    ) -> Vec<&'a T> {
        if filter.is_empty() {
            return items.collect();
        }
        let mut scored: Vec<(i64, &T)> = items
            .filter_map(|it| self.matcher.fuzzy_match(&text(it), filter).map(|s| (s, it)))
            .collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        scored.into_iter().map(|(_, it)| it).collect()
    }

    pub fn visible_projects(&self) -> Vec<&Project> {
        self.filter_rank(
            self.projects.iter(),
            |p| format!("{} {}", p.name, p.kind.label()),
            &self.filters[0],
        )
    }

    pub fn visible_ports(&self) -> Vec<&Listener> {
        let all = self.show_all_ports;
        self.filter_rank(
            self.ports.iter().filter(|l| all || !l.system),
            |l| {
                format!(
                    "{} {} {} {}",
                    l.port,
                    l.process,
                    l.project.as_deref().unwrap_or(""),
                    l.command
                )
            },
            &self.filters[1],
        )
    }

    pub fn visible_deploys(&self) -> Vec<&Deploy> {
        self.filter_rank(
            self.deploys.deploys.iter(),
            |d| {
                format!(
                    "{} {} {} {}",
                    d.project,
                    d.title,
                    d.detail.as_deref().unwrap_or(""),
                    d.state.label()
                )
            },
            &self.filters[2],
        )
    }

    fn row_count(&self, tab: Tab) -> usize {
        match tab {
            Tab::Projects => self.visible_projects().len(),
            Tab::Ports => self.visible_ports().len(),
            Tab::Deploys => self.visible_deploys().len(),
        }
    }

    fn key_at(&self, tab: Tab, i: usize) -> Option<String> {
        match tab {
            Tab::Projects => self
                .visible_projects()
                .get(i)
                .map(|p| p.path.display().to_string()),
            Tab::Ports => self
                .visible_ports()
                .get(i)
                .map(|l| format!("{}:{}", l.port, l.pid)),
            Tab::Deploys => self.visible_deploys().get(i).map(|d| d.id.clone()),
        }
    }

    fn index_of_key(&self, tab: Tab, key: &str) -> Option<usize> {
        (0..self.row_count(tab)).find(|&i| self.key_at(tab, i).as_deref() == Some(key))
    }

    pub fn selected_project(&self) -> Option<&Project> {
        self.visible_projects().get(self.selected[0]).copied()
    }

    pub fn selected_port(&self) -> Option<&Listener> {
        self.visible_ports().get(self.selected[1]).copied()
    }

    pub fn selected_deploy(&self) -> Option<&Deploy> {
        self.visible_deploys().get(self.selected[2]).copied()
    }

    fn select(&mut self, tab: Tab, i: usize) {
        let n = self.row_count(tab);
        let i = if n == 0 { 0 } else { i.min(n - 1) };
        self.selected[tab.index()] = i;
        self.selected_key[tab.index()] = self.key_at(tab, i);
    }

    /// After data changes, keep the same item selected if it still exists.
    fn restore_selection(&mut self, tab: Tab) {
        let t = tab.index();
        let i = self.selected_key[t]
            .clone()
            .and_then(|k| self.index_of_key(tab, &k))
            .unwrap_or(self.selected[t]);
        self.select(tab, i);
    }

    // ---------- Cross-references ----------

    pub fn run_for(&self, project: &str) -> Option<&Run> {
        self.runs.iter().find(|r| r.project == project)
    }

    pub fn ports_for(&self, project: &str) -> Vec<&Listener> {
        self.ports
            .iter()
            .filter(|l| l.project.as_deref() == Some(project))
            .collect()
    }

    pub fn project_named(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    fn jump_to_project(&mut self, name: &str) {
        let Some(path) = self
            .project_named(name)
            .map(|p| p.path.display().to_string())
        else {
            self.set_status(Level::Err, format!("{name} isn't in your project folders"));
            return;
        };
        self.filters[0].clear();
        self.tab = Tab::Projects;
        if let Some(i) = self.index_of_key(Tab::Projects, &path) {
            self.select(Tab::Projects, i);
        }
    }

    // ---------- Events from workers ----------

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Projects(list) => {
                self.projects = list;
                self.loaded.projects = true;
                self.updated[0] = Some(Instant::now());
                self.restore_selection(Tab::Projects);
            }
            Event::Ports(res) => {
                match res {
                    Ok(list) => {
                        self.ports = list;
                        self.ports_error = None;
                    }
                    Err(e) => self.ports_error = Some(e),
                }
                self.loaded.ports = true;
                self.updated[1] = Some(Instant::now());
                self.restore_selection(Tab::Ports);
            }
            Event::Deploys(report) => {
                self.deploys = report;
                self.loaded.deploys = true;
                self.updated[2] = Some(Instant::now());
                self.restore_selection(Tab::Deploys);
            }
            Event::Runs(runs) => self.runs = runs,
            Event::ActionDone {
                ok,
                message,
                refresh,
            } => {
                if !self.busy.is_empty() {
                    self.busy.remove(0);
                }
                self.set_status(if ok { Level::Ok } else { Level::Err }, message);
                self.refresh(refresh);
            }
        }
    }

    /// Periodic housekeeping: expire old status messages and re-read an open log.
    pub fn tick(&mut self) {
        if self
            .status
            .as_ref()
            .is_some_and(|s| s.at.elapsed() > Duration::from_secs(6))
        {
            self.status = None;
        }
        if let Some(Popup::Logs {
            path,
            lines,
            read_at,
            follow,
            scroll,
            ..
        }) = &mut self.popup
            && read_at.is_none_or(|t| t.elapsed() > Duration::from_millis(500))
        {
            *lines = runner::tail(path, LOG_LINES);
            *read_at = Some(Instant::now());
            if *follow {
                *scroll = usize::MAX;
            }
        }
    }

    // ---------- Keys ----------

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
        {
            self.quit = true;
            return None;
        }
        if self.popup.is_some() {
            self.on_popup_key(key);
            return None;
        }
        if self.editing_filter {
            self.on_filter_key(key);
            return None;
        }
        let t = self.tab;
        let page = 10;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
            KeyCode::Char('/') => self.editing_filter = true,
            KeyCode::Esc => {
                if !self.filters[t.index()].is_empty() {
                    self.filters[t.index()].clear();
                    self.select(t, 0);
                }
            }
            KeyCode::Tab | KeyCode::Right => self.tab = Tab::ALL[(t.index() + 1) % 3],
            KeyCode::BackTab | KeyCode::Left => self.tab = Tab::ALL[(t.index() + 2) % 3],
            KeyCode::Char('1') => self.tab = Tab::Projects,
            KeyCode::Char('2') => self.tab = Tab::Ports,
            KeyCode::Char('3') => self.tab = Tab::Deploys,
            KeyCode::Down | KeyCode::Char('j') => self.select(t, self.selected[t.index()] + 1),
            KeyCode::Up | KeyCode::Char('k') => {
                self.select(t, self.selected[t.index()].saturating_sub(1))
            }
            KeyCode::PageDown => self.select(t, self.selected[t.index()] + page),
            KeyCode::PageUp => self.select(t, self.selected[t.index()].saturating_sub(page)),
            KeyCode::Home | KeyCode::Char('g') => self.select(t, 0),
            KeyCode::End | KeyCode::Char('G') => self.select(t, usize::MAX),
            KeyCode::Char('r') => {
                self.refresh(match t {
                    Tab::Projects => Refresh {
                        projects: true,
                        ..Default::default()
                    },
                    Tab::Ports => Refresh::PORTS,
                    Tab::Deploys => Refresh::DEPLOYS,
                });
                self.set_status(
                    Level::Info,
                    format!("Refreshing {}…", t.title().to_lowercase()),
                );
            }
            KeyCode::Char('R') => {
                self.refresh(Refresh::ALL);
                self.set_status(Level::Info, "Refreshing everything…");
            }
            _ => {
                return match t {
                    Tab::Projects => self.on_projects_key(key),
                    Tab::Ports => {
                        self.on_ports_key(key);
                        None
                    }
                    Tab::Deploys => {
                        self.on_deploys_key(key);
                        None
                    }
                };
            }
        }
        None
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        let t = self.tab.index();
        match key.code {
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Esc => {
                self.filters[t].clear();
                self.editing_filter = false;
            }
            KeyCode::Backspace => {
                self.filters[t].pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filters[t].clear()
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.filters[t].chars().count() < 64 {
                    self.filters[t].push(c);
                }
            }
            KeyCode::Down => self.select(self.tab, self.selected[t] + 1),
            KeyCode::Up => self.select(self.tab, self.selected[t].saturating_sub(1)),
            _ => {}
        }
        self.select(
            self.tab,
            if matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace) {
                0
            } else {
                self.selected[t]
            },
        );
    }

    pub fn on_paste(&mut self, text: &str) {
        if self.editing_filter {
            let t = self.tab.index();
            self.filters[t].extend(text.chars().filter(|c| !c.is_control()).take(64));
            self.select(self.tab, 0);
        }
    }

    fn on_popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        match popup {
            Popup::Help => {}
            Popup::Confirm {
                action,
                title,
                body,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.perform(action),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => {
                    self.popup = Some(Popup::Confirm {
                        title,
                        body,
                        action,
                    })
                }
            },
            Popup::Logs {
                title,
                path,
                lines,
                mut scroll,
                mut follow,
                read_at,
            } => {
                let max = lines.len().saturating_sub(1);
                let cur = scroll.min(max);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => return,
                    KeyCode::Down | KeyCode::Char('j') => scroll = (cur + 1).min(max),
                    KeyCode::Up | KeyCode::Char('k') => scroll = cur.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => scroll = (cur + 20).min(max),
                    KeyCode::PageUp => scroll = cur.saturating_sub(20),
                    KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                    KeyCode::End | KeyCode::Char('G') | KeyCode::Char('f') => {
                        follow = true;
                        scroll = usize::MAX;
                    }
                    _ => {}
                }
                if !matches!(
                    key.code,
                    KeyCode::End | KeyCode::Char('G') | KeyCode::Char('f')
                ) && scroll != usize::MAX
                {
                    follow = scroll >= max;
                }
                self.popup = Some(Popup::Logs {
                    title,
                    path,
                    lines,
                    scroll,
                    follow,
                    read_at,
                });
            }
        }
    }

    fn on_projects_key(&mut self, key: KeyEvent) -> Option<Effect> {
        let p = self.selected_project()?.clone();
        match key.code {
            KeyCode::Enter | KeyCode::Char('o') | KeyCode::Char('e') => {
                return self.open_in_editor(&p.path);
            }
            KeyCode::Char('s') => self.toggle_server(&p),
            KeyCode::Char('l') => self.show_logs(&p.name),
            KeyCode::Char('b') => match p.remote() {
                Some(r) => self.open_url(&r.web_url()),
                None => self.set_status(Level::Err, format!("{} has no git remote", p.name)),
            },
            KeyCode::Char('v') => {
                match self.deploys.latest_for(&p.name).and_then(|d| d.url.clone()) {
                    Some(url) => self.open_url(&url),
                    None => self.set_status(Level::Err, format!("no deployments for {}", p.name)),
                }
            }
            KeyCode::Char('u') => match self.ports_for(&p.name).first().map(|l| l.url()) {
                Some(url) => self.open_url(&url),
                None => self.set_status(
                    Level::Err,
                    format!("{} isn't listening on any port", p.name),
                ),
            },
            KeyCode::Char('f') => self.reveal(&p.path),
            KeyCode::Char('t') => self.open_terminal(&p.path),
            KeyCode::Char('c') => self.copy(&p.path.display().to_string()),
            KeyCode::Char('D') => {
                if p.vercel.is_none() {
                    self.set_status(
                        Level::Err,
                        format!(
                            "{} isn't linked to Vercel (run `vercel link` in it)",
                            p.name
                        ),
                    );
                } else if util::which("vercel").is_none() {
                    self.set_status(Level::Err, "the `vercel` CLI isn't installed");
                } else {
                    self.popup = Some(Popup::Confirm {
                        title: "Deploy to production?".into(),
                        body: format!(
                            "Run `vercel --prod` for {}?\n{}",
                            p.name,
                            util::tilde(&p.path)
                        ),
                        action: Action::Deploy {
                            project: p.name.clone(),
                            path: p.path.clone(),
                        },
                    });
                }
            }
            _ => {}
        }
        None
    }

    fn on_ports_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('a') {
            self.show_all_ports = !self.show_all_ports;
            self.select(Tab::Ports, 0);
            self.set_status(
                Level::Info,
                if self.show_all_ports {
                    "Showing all listeners"
                } else {
                    "Showing dev servers only"
                },
            );
            return;
        }
        let Some(l) = self.selected_port().cloned() else {
            return;
        };
        match key.code {
            KeyCode::Char('x') | KeyCode::Char('X') | KeyCode::Delete => {
                let force = key.code == KeyCode::Char('X');
                let what = if force {
                    "Force-kill (SIGKILL)"
                } else {
                    "Stop (SIGTERM)"
                };
                self.popup = Some(Popup::Confirm {
                    title: format!("{what} :{}?", l.port),
                    body: format!(
                        "{} (pid {})\n{}",
                        l.process,
                        l.pid,
                        util::truncate(&l.command, 200)
                    ),
                    action: Action::Kill {
                        pid: l.pid,
                        port: l.port,
                        process: l.process.clone(),
                        force,
                    },
                });
            }
            KeyCode::Enter | KeyCode::Char('o') => self.open_url(&l.url()),
            KeyCode::Char('p') => match &l.project {
                Some(name) => self.jump_to_project(&name.clone()),
                None => self.set_status(Level::Err, "not attributed to a project"),
            },
            KeyCode::Char('c') => self.copy(&l.url()),
            KeyCode::Char('l') => match l.project.clone().filter(|n| self.run_for(n).is_some()) {
                Some(name) => self.show_logs(&name),
                None => self.set_status(Level::Err, "only servers started by hangar have logs"),
            },
            _ => {}
        }
    }

    fn on_deploys_key(&mut self, key: KeyEvent) {
        let Some(d) = self.selected_deploy().cloned() else {
            return;
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char('o') => match &d.url {
                Some(url) => self.open_url(url),
                None => self.set_status(Level::Err, "no URL for this entry"),
            },
            KeyCode::Char('i') => match d.inspect_url.as_ref().or(d.url.as_ref()) {
                Some(url) => self.open_url(&url.clone()),
                None => self.set_status(Level::Err, "no inspector link"),
            },
            KeyCode::Char('p') => self.jump_to_project(&d.project),
            KeyCode::Char('c') => match &d.url {
                Some(url) => self.copy(&url.clone()),
                None => self.set_status(Level::Err, "no URL to copy"),
            },
            _ => {}
        }
    }

    // ---------- Actions ----------

    fn spawn_action(
        &mut self,
        label: String,
        refresh: Refresh,
        f: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        self.busy.push(label.clone());
        self.set_status(Level::Info, format!("{label}…"));
        let tx = self.events.clone();
        // Named `hangar-*` so the panic hook knows not to tear down the terminal.
        let spawned = std::thread::Builder::new()
            .name("hangar-action".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                    .unwrap_or_else(|_| Err(format!("{label}: crashed")));
                let (ok, message) = match res {
                    Ok(m) => (true, m),
                    Err(m) => (false, m),
                };
                let _ = tx.send(Event::ActionDone {
                    ok,
                    message,
                    refresh,
                });
            });
        if let Err(e) = spawned {
            self.busy.pop();
            self.set_status(Level::Err, format!("couldn't start background task: {e}"));
        }
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Kill {
                pid,
                port,
                process,
                force,
            } => {
                self.spawn_action(format!("Stopping :{port}"), Refresh::PORTS, move || {
                    match ports::kill_pid(pid, force, Duration::from_secs(3)) {
                        Ok(o) if o == ports::KillOutcome::StillRunning => Err(format!(
                            "{process} (pid {pid}) is {} — press X",
                            o.describe()
                        )),
                        Ok(o) => Ok(format!("{process} on :{port} {}", o.describe())),
                        Err(e) => Err(format!("{process}: {e}")),
                    }
                });
            }
            Action::StopServer { project } => {
                let runner = self.runner.clone();
                self.spawn_action(format!("Stopping {project}"), Refresh::PORTS, move || {
                    runner
                        .stop(&project)
                        .map(|_| format!("Stopped {project}"))
                        .map_err(|e| e.to_string())
                });
            }
            Action::Deploy { project, path } => {
                let log = self
                    .runner
                    .logs_dir()
                    .join(format!("{}-deploy.log", project.replace(['/', ' '], "-")));
                self.spawn_action(
                    format!("Deploying {project}"),
                    Refresh::DEPLOYS,
                    move || deploy(&project, &path, &log),
                );
                self.refresh(Refresh::DEPLOYS);
            }
        }
    }

    fn toggle_server(&mut self, p: &Project) {
        if self.run_for(&p.name).is_some() {
            self.popup = Some(Popup::Confirm {
                title: format!("Stop {}?", p.name),
                body: "Stops the dev server and everything it started.".into(),
                action: Action::StopServer {
                    project: p.name.clone(),
                },
            });
            return;
        }
        let Some(cmd) = p.dev_command.clone() else {
            self.set_status(
                Level::Err,
                format!("don't know how to start {} (no dev/start script)", p.name),
            );
            return;
        };
        if let Some(l) = self.ports_for(&p.name).first() {
            self.set_status(
                Level::Err,
                format!(
                    "{} is already serving on :{} (pid {})",
                    p.name, l.port, l.pid
                ),
            );
            return;
        }
        let runner = self.runner.clone();
        let (name, path) = (p.name.clone(), p.path.clone());
        self.spawn_action(format!("Starting {name}"), Refresh::PORTS, move || {
            runner
                .start(&name, &path, &cmd)
                .map(|r| {
                    format!(
                        "Started `{}` for {name} (pid {}) — press l for logs",
                        cmd.join(" "),
                        r.pid
                    )
                })
                .map_err(|e| format!("{name}: {e}"))
        });
    }

    fn show_logs(&mut self, project: &str) {
        let path = self
            .run_for(project)
            .map(|r| r.log.clone())
            .unwrap_or_else(|| self.runner.log_path(project));
        if !path.exists() {
            self.set_status(
                Level::Err,
                format!("no logs for {project} yet (start it with s)"),
            );
            return;
        }
        self.popup = Some(Popup::Logs {
            title: format!("{project} — {}", util::tilde(&path)),
            path,
            lines: vec![],
            scroll: usize::MAX,
            follow: true,
            read_at: None,
        });
        self.tick();
    }

    fn open_url(&mut self, url: &str) {
        match open::that_detached(url) {
            Ok(()) => self.set_status(Level::Ok, format!("Opened {url}")),
            Err(e) => self.set_status(Level::Err, format!("couldn't open {url}: {e}")),
        }
    }

    fn reveal(&mut self, path: &Path) {
        match open::that_detached(path) {
            Ok(()) => self.set_status(Level::Ok, format!("Opened {}", util::tilde(path))),
            Err(e) => self.set_status(Level::Err, format!("couldn't open folder: {e}")),
        }
    }

    fn open_terminal(&mut self, path: &Path) {
        let res = if cfg!(target_os = "macos") {
            util::spawn_detached(Command::new("open").args(["-a", "Terminal"]).arg(path))
        } else if let Some(term) = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty()) {
            util::spawn_detached(Command::new(term).current_dir(path))
        } else {
            util::spawn_detached(Command::new("x-terminal-emulator").current_dir(path))
        };
        match res {
            Ok(()) => self.set_status(
                Level::Ok,
                format!("Opened a terminal in {}", util::tilde(path)),
            ),
            Err(e) => self.set_status(Level::Err, e.to_string()),
        }
    }

    fn copy(&mut self, text: &str) {
        match util::copy_to_clipboard(text) {
            Ok(()) => self.set_status(Level::Ok, format!("Copied {text}")),
            Err(e) => self.set_status(Level::Err, e.to_string()),
        }
    }

    /// GUI editors launch in the background; terminal editors take over the screen until they exit.
    fn open_in_editor(&mut self, path: &Path) -> Option<Effect> {
        let editor = self.cfg.editor_command();
        let mut parts = editor.split_whitespace();
        let program = parts.next()?.to_string();
        let mut cmd = Command::new(&program);
        cmd.args(parts).arg(path).current_dir(path);
        let base = Path::new(&program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if [
            "vi", "vim", "nvim", "nano", "hx", "helix", "emacs", "micro", "kak", "pico", "joe",
        ]
        .contains(&base.as_str())
        {
            return Some(Effect::Foreground(cmd));
        }
        match util::spawn_detached(&mut cmd) {
            Ok(()) => self.set_status(
                Level::Ok,
                format!("Opened {} in {program}", util::tilde(path)),
            ),
            Err(e) => self.set_status(Level::Err, format!("{e} — set `editor` in the config")),
        }
        None
    }
}

/// `vercel --prod --yes`, logging to a file and returning the deployment URL.
fn deploy(project: &str, path: &Path, log: &Path) -> Result<String, String> {
    let out = util::run(
        Command::new("vercel")
            .args(["--prod", "--yes", "--no-color"])
            .current_dir(path),
        Duration::from_secs(15 * 60),
    )
    .map_err(|e| format!("{project}: {e}"))?;
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(log, format!("{}\n{}", out.stdout, out.stderr));
    if !out.success() {
        return Err(format!("{project} deploy failed: {}", out.error_line()));
    }
    let url = format!("{}\n{}", out.stdout, out.stderr)
        .split_whitespace()
        .rfind(|w| w.starts_with("https://") && w.contains(".vercel.app"))
        .map(String::from);
    Ok(match url {
        Some(u) => format!("Deployed {project}: {u}"),
        None => format!("Deployed {project}"),
    })
}

/// Restores the terminal on panic, but only for the UI thread: a worker panic is caught and reported
/// in the UI instead of tearing the screen down mid-session.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current()
            .name()
            .is_some_and(|n| n.starts_with("hangar-"))
        {
            return;
        }
        previous(info);
    }));
}

/// Runs the dashboard until the user quits.
pub fn run(cfg: Config) -> Result<()> {
    let cfg = Arc::new(cfg);
    let runner = Arc::new(Runner::default());
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(cfg.clone(), tx.clone(), runner.clone());

    let mut terminal = ratatui::try_init()?;
    install_panic_hook();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableBracketedPaste
    );
    app.attach_workers(worker::spawn(cfg, tx, runner));

    let result = (|| -> Result<()> {
        loop {
            while let Ok(ev) = rx.try_recv() {
                app.on_event(ev);
            }
            app.tick();
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if app.quit {
                return Ok(());
            }
            if !event::poll(Duration::from_millis(200))? {
                continue;
            }
            match event::read()? {
                TermEvent::Key(key) => {
                    if let Some(Effect::Foreground(mut cmd)) = app.on_key(key) {
                        let _ = ratatui::crossterm::execute!(
                            std::io::stdout(),
                            ratatui::crossterm::event::DisableBracketedPaste
                        );
                        ratatui::restore();
                        let status = cmd.status();
                        terminal = ratatui::try_init()?;
                        let _ = ratatui::crossterm::execute!(
                            std::io::stdout(),
                            ratatui::crossterm::event::EnableBracketedPaste
                        );
                        terminal.clear()?;
                        if let Err(e) = status {
                            app.set_status(Level::Err, format!("couldn't run editor: {e}"));
                        }
                    }
                }
                TermEvent::Paste(text) => app.on_paste(&text),
                _ => {}
            }
        }
    })();

    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    result
}
