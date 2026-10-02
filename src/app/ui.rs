//! Rendering. Pure function of [`App`] state, so it can be tested with ratatui's `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};

use super::{App, Level, Popup, Tab};
use crate::deploys::{Deploy, Source, SourceStatus, State};
use crate::ports::Listener;
use crate::projects::Project;
use crate::util;

const ACCENT: Color = Color::Indexed(75);
const DIM: Color = Color::Indexed(244);
const FAINT: Color = Color::Indexed(240);
const HIGHLIGHT_BG: Color = Color::Indexed(237);

fn state_color(s: State) -> Color {
    match s {
        State::Success => Color::Green,
        State::Failed => Color::Red,
        State::Running => Color::Yellow,
        State::Queued => Color::Cyan,
        State::Canceled | State::Unknown => DIM,
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 30 || area.height < 6 {
        f.render_widget(Paragraph::new("hangar: enlarge the terminal").fg(DIM), area);
        return;
    }
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(f, app, header);
    draw_body(f, app, body);
    draw_footer(f, app, footer);
    match app.popup.clone() {
        Some(Popup::Help) => draw_help(f, area),
        Some(Popup::Confirm { title, body, .. }) => draw_confirm(f, area, &title, &body),
        Some(Popup::Logs {
            title,
            lines,
            scroll,
            follow,
            ..
        }) => draw_logs(f, area, &title, &lines, scroll, follow),
        None => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled(
            " ⛭ hangar ",
            Style::new().fg(Color::Black).bg(ACCENT).bold(),
        ),
        Span::raw(" "),
    ];
    for tab in Tab::ALL {
        let count = match tab {
            Tab::Projects => app.visible_projects().len(),
            Tab::Ports => app.visible_ports().len(),
            Tab::Deploys => app.visible_deploys().len(),
        };
        let label = format!(" {} {} {} ", tab.index() + 1, tab.title(), count);
        let style = if tab == app.tab {
            Style::new().fg(Color::White).bg(HIGHLIGHT_BG).bold()
        } else {
            Style::new().fg(DIM)
        };
        spans.push(Span::styled(label, style));
    }
    f.render_widget(Line::from(spans), area);

    let mut right: Vec<Span> = vec![];
    let servers = app.ports.iter().filter(|l| l.project.is_some()).count();
    if servers > 0 {
        right.push(Span::styled(
            format!("● {servers} serving  "),
            Style::new().fg(Color::Green),
        ));
    }
    let active = app.deploys.active_count();
    if active > 0 {
        right.push(Span::styled(
            format!("◐ {active} building  "),
            Style::new().fg(Color::Yellow),
        ));
    }
    let failing = app.deploys.failing().len();
    if failing > 0 {
        right.push(Span::styled(
            format!("✗ {failing} failing  "),
            Style::new().fg(Color::Red),
        ));
    }
    let right = Line::from(right).alignment(Alignment::Right);
    let used: u16 = Tab::ALL
        .iter()
        .map(|t| t.title().len() as u16 + 8)
        .sum::<u16>()
        + 11;
    if area.width > used + right.width() as u16 {
        f.render_widget(right, area);
    }
}

fn draw_body(f: &mut Frame, app: &mut App, area: Rect) {
    let wide = area.width >= 120;
    let (table_area, detail_area) = if wide {
        let [a, b] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
            .areas(area);
        (a, Some(b))
    } else if area.height >= 18 {
        // Narrow terminals stack the details under the table, giving the table most of the room.
        let details = (area.height * 2 / 5).clamp(7, 12);
        let [a, b] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(details)]).areas(area);
        (a, Some(b))
    } else {
        (area, None)
    };
    match app.tab {
        Tab::Projects => projects_table(f, app, table_area),
        Tab::Ports => ports_table(f, app, table_area),
        Tab::Deploys => deploys_table(f, app, table_area),
    }
    if let Some(d) = detail_area {
        let width = d.width.saturating_sub(2) as usize;
        let lines = match app.tab {
            Tab::Projects => app
                .selected_project()
                .map(|p| project_details(app, p, width)),
            Tab::Ports => app.selected_port().map(|l| port_details(app, l)),
            Tab::Deploys => app.selected_deploy().map(deploy_details),
        }
        .unwrap_or_else(|| empty_details(app));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(FAINT))
            .title(" Details ".fg(DIM));
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            d,
        );
    }
}

fn table_block(app: &App, title: &str) -> Block<'static> {
    let filter = &app.filters[app.tab.index()];
    let mut t = vec![Span::styled(
        format!(" {title} "),
        Style::new().fg(Color::White).bold(),
    )];
    if !filter.is_empty() || app.editing_filter {
        t.push(Span::styled(
            format!("/{filter} "),
            Style::new().fg(Color::Yellow),
        ));
    }
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(FAINT))
        .title(Line::from(t))
}

struct TableSpec<'a> {
    title: &'a str,
    header: Vec<&'a str>,
    widths: Vec<Constraint>,
    rows: Vec<Row<'static>>,
    empty: Vec<Line<'static>>,
}

fn render_table(f: &mut Frame, app: &App, area: Rect, spec: TableSpec) {
    let TableSpec {
        title,
        header,
        widths,
        rows,
        empty,
    } = spec;
    let block = table_block(app, title);
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(empty)
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }
    let header = Row::new(header.into_iter().map(|h| Cell::from(h.to_string())))
        .style(Style::new().fg(DIM).add_modifier(Modifier::BOLD));
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(HIGHLIGHT_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    let mut state = TableState::default().with_selected(Some(app.selected[app.tab.index()]));
    f.render_stateful_widget(table, area, &mut state);
}

fn empty_message(loaded: bool, loading: &str, lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if loaded {
        lines
    } else {
        vec![Line::from(""), Line::from(format!("  {loading}").fg(DIM))]
    }
}

// ---------- Projects ----------

fn changes_cell(p: &Project) -> Span<'static> {
    match (&p.git, &p.git_error) {
        (Some(g), _) if g.conflicted > 0 => Span::styled(
            format!("{} conflict", g.conflicted),
            Style::new().fg(Color::Red),
        ),
        (Some(g), _) if g.dirty() => {
            let n = g.staged + g.modified + g.untracked;
            let label = if n > 999 {
                "999+ changed".to_string()
            } else {
                format!("{n} changed")
            };
            Span::styled(label, Style::new().fg(Color::Yellow))
        }
        (Some(_), _) => Span::styled("clean", Style::new().fg(Color::Green)),
        (None, Some(_)) => Span::styled("git error", Style::new().fg(Color::Red)),
        (None, None) => Span::styled("no git", Style::new().fg(FAINT)),
    }
}

fn sync_cell(p: &Project) -> Span<'static> {
    let Some(g) = &p.git else {
        return Span::raw("");
    };
    match (g.upstream.is_some(), g.ahead, g.behind) {
        (false, _, _) if g.remote.is_some() => {
            Span::styled("unpushed", Style::new().fg(Color::Magenta))
        }
        (false, _, _) => Span::raw(""),
        (true, 0, 0) => Span::styled("✓", Style::new().fg(FAINT)),
        (true, a, b) => {
            let mut s = String::new();
            if a > 0 {
                s.push_str(&format!("↑{a}"));
            }
            if b > 0 {
                s.push_str(&format!("↓{b}"));
            }
            Span::styled(s, Style::new().fg(Color::Cyan))
        }
    }
}

fn deploy_cell(d: Option<&Deploy>) -> Span<'static> {
    match d {
        Some(d) => Span::styled(
            format!("{} {}", d.state.symbol(), util::ago(d.created)),
            Style::new().fg(state_color(d.state)),
        ),
        None => Span::raw(""),
    }
}

fn projects_table(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .visible_projects()
        .into_iter()
        .map(|p| {
            let serving = !app.ports_for(&p.name).is_empty();
            let dot = if serving {
                Span::styled("●", Style::new().fg(Color::Green))
            } else if app.run_for(&p.name).is_some() {
                Span::styled("◌", Style::new().fg(Color::Yellow))
            } else {
                Span::raw(" ")
            };
            let branch = p
                .git
                .as_ref()
                .map(|g| g.branch.clone().unwrap_or_else(|| "(detached)".into()))
                .unwrap_or_default();
            let last = p
                .git
                .as_ref()
                .and_then(|g| g.last_commit_time())
                .map(util::ago)
                .unwrap_or_default();
            Row::new(vec![
                Cell::from(dot),
                Cell::from(p.name.clone()),
                Cell::from(Span::styled(p.kind.label(), Style::new().fg(DIM))),
                Cell::from(Span::styled(branch, Style::new().fg(Color::Magenta))),
                Cell::from(changes_cell(p)),
                Cell::from(sync_cell(p)),
                Cell::from(Span::styled(last, Style::new().fg(DIM))),
                Cell::from(deploy_cell(app.deploys.latest_for(&p.name))),
            ])
        })
        .collect();
    let empty = empty_message(
        app.loaded.projects,
        "Scanning project folders…",
        if app.projects.is_empty() {
            let roots: Vec<String> = app.cfg.roots.iter().map(|r| util::tilde(r)).collect();
            vec![
                Line::from(""),
                Line::from(format!("  No projects found in {}.", if roots.is_empty() { "any folder".into() } else { roots.join(", ") })),
                Line::from("  Point hangar at your code with `hangar config --init` or `hangar --root ~/code`.".fg(DIM)),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from("  No matches. Esc clears the filter.".fg(DIM)),
            ]
        },
    );
    render_table(
        f,
        app,
        area,
        TableSpec {
            title: "Projects",
            header: vec![
                "", "Name", "Kind", "Branch", "Changes", "Sync", "Last", "Deploy",
            ],
            widths: vec![
                Constraint::Length(1),
                Constraint::Min(14),
                Constraint::Length(6),
                Constraint::Max(18),
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Length(4),
                Constraint::Length(7),
            ],
            rows,
            empty,
        },
    );
}

/// Details pane content when nothing is selected.
fn empty_details(app: &App) -> Vec<Line<'static>> {
    match app.tab {
        Tab::Deploys => source_summary(app),
        Tab::Ports => vec![
            Line::from(""),
            Line::from(" Listening TCP ports and the process behind each.".fg(DIM)),
            Line::from(" A port belongs to a project when its process".fg(DIM)),
            Line::from(" runs inside that project's folder.".fg(DIM)),
        ],
        Tab::Projects => {
            let mut l = vec![Line::from(""), Line::from(" Scanning:".fg(DIM))];
            l.extend(
                app.cfg
                    .roots
                    .iter()
                    .map(|r| Line::from(format!("  {}", util::tilde(r)))),
            );
            l
        }
    }
}

fn project_details(app: &App, p: &Project, width: usize) -> Vec<Line<'static>> {
    let mut l: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(p.name.clone(), Style::new().bold().fg(Color::White)),
            Span::styled(format!("  {}", p.kind.label()), Style::new().fg(DIM)),
        ]),
        Line::from(util::tilde(&p.path).fg(DIM)),
        Line::from(""),
    ];
    let ports = app.ports_for(&p.name);
    if let Some(run) = app.run_for(&p.name) {
        l.push(Line::from(vec![
            "● ".green(),
            Span::raw(format!("{} ", run.command.join(" "))),
            Span::styled(
                format!("pid {} · up {}", run.pid, util::ago(run.started)),
                Style::new().fg(DIM),
            ),
        ]));
    }
    for port in &ports {
        l.push(Line::from(vec![
            "  ↳ ".fg(DIM),
            Span::styled(port.url(), Style::new().fg(ACCENT).underlined()),
            format!("  {}", port.process).fg(DIM),
        ]));
    }
    if ports.is_empty() && app.run_for(&p.name).is_none() {
        match &p.dev_command {
            Some(cmd) => l.push(Line::from(vec![
                "○ ".fg(DIM),
                format!("s to run `{}`", cmd.join(" ")).fg(DIM),
            ])),
            None => l.push(Line::from("○ no dev command detected".fg(DIM))),
        }
    }
    l.push(Line::from(""));

    match (&p.git, &p.git_error) {
        (Some(g), _) => {
            let branch = g
                .branch
                .clone()
                .unwrap_or_else(|| format!("detached at {}", g.head.clone().unwrap_or_default()));
            let mut line = vec![
                " ".into(),
                Span::styled(branch, Style::new().fg(Color::Magenta).bold()),
            ];
            if let Some(up) = &g.upstream {
                line.push(format!(" → {up}").fg(DIM));
            }
            if g.ahead > 0 {
                line.push(format!("  ↑{} to push", g.ahead).cyan());
            }
            if g.behind > 0 {
                line.push(format!("  ↓{} to pull", g.behind).cyan());
            }
            l.push(Line::from(line));
            if g.dirty() {
                let mut parts = vec![];
                for (n, label) in [
                    (g.staged, "staged"),
                    (g.modified, "modified"),
                    (g.untracked, "untracked"),
                    (g.conflicted, "conflicted"),
                ] {
                    if n > 0 {
                        parts.push(format!("{n} {label}"));
                    }
                }
                l.push(Line::from(format!("  {}", parts.join(", ")).yellow()));
            } else {
                l.push(Line::from("  working tree clean".green()));
            }
            for c in &g.commits {
                // One line per commit: sha, subject truncated to fit the pane, age.
                let age = util::ago(c.time);
                let room = width
                    .saturating_sub(c.sha.chars().count() + age.chars().count() + 4)
                    .max(8);
                l.push(Line::from(vec![
                    Span::styled(format!("  {} ", c.sha), Style::new().fg(Color::Yellow)),
                    Span::raw(util::truncate(&c.subject, room)),
                    Span::styled(format!(" {age}"), Style::new().fg(DIM)),
                ]));
            }
            if g.commits.is_empty() {
                l.push(Line::from("  no commits yet".fg(DIM)));
            }
            if let Some(r) = &g.remote {
                l.push(Line::from(vec![
                    "  ".into(),
                    Span::styled(r.web_url(), Style::new().fg(ACCENT)),
                ]));
            } else {
                l.push(Line::from("  no remote".fg(DIM)));
            }
        }
        (None, Some(e)) => l.push(Line::from(format!(" git: {e}").red())),
        (None, None) => l.push(Line::from(" not a git repository".fg(DIM))),
    }

    if p.vercel.is_some() || app.deploys.latest_for(&p.name).is_some() {
        l.push(Line::from(""));
        match app.deploys.latest_for(&p.name) {
            Some(d) => {
                l.push(Line::from(vec![
                    Span::styled(
                        format!("{} {} ", d.state.symbol(), d.state.label()),
                        Style::new().fg(state_color(d.state)).bold(),
                    ),
                    Span::styled(
                        format!("{} · {} ago", d.source.label(), util::ago(d.created)),
                        Style::new().fg(DIM),
                    ),
                ]));
                if let Some(u) = &d.url {
                    l.push(Line::from(vec![
                        "  ".into(),
                        Span::styled(u.clone(), Style::new().fg(ACCENT)),
                    ]));
                }
            }
            None => l.push(Line::from(
                " ▲ linked to Vercel, no deployments loaded".fg(DIM),
            )),
        }
    }
    l
}

// ---------- Ports ----------

fn ports_table(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .visible_ports()
        .into_iter()
        .map(|l| {
            let started_by_us = app
                .runs
                .iter()
                .any(|r| Some(&r.project) == l.project.as_ref());
            Row::new(vec![
                Cell::from(Span::styled(
                    format!(":{}", l.port),
                    Style::new().fg(ACCENT).bold(),
                )),
                Cell::from(l.process.clone()),
                Cell::from(Span::styled(l.pid.to_string(), Style::new().fg(DIM))),
                Cell::from(match &l.project {
                    Some(p) => Span::styled(
                        format!("{p}{}", if started_by_us { " ◆" } else { "" }),
                        Style::new().fg(Color::Green),
                    ),
                    None => Span::styled("-", Style::new().fg(FAINT)),
                }),
                Cell::from(Span::styled(
                    l.started.map(util::ago).unwrap_or_default(),
                    Style::new().fg(DIM),
                )),
                Cell::from(if l.memory > 0 {
                    util::bytes(l.memory)
                } else {
                    String::new()
                }),
                Cell::from(Span::styled(
                    if l.cpu >= 0.1 {
                        format!("{:.0}%", l.cpu)
                    } else {
                        String::new()
                    },
                    Style::new().fg(DIM),
                )),
                Cell::from(Span::styled(
                    if l.local_only() { "local" } else { "network" },
                    Style::new().fg(if l.local_only() { DIM } else { Color::Yellow }),
                )),
            ])
        })
        .collect();
    let mut empty = vec![Line::from("")];
    if let Some(e) = &app.ports_error {
        empty.push(Line::from(format!("  Couldn't list ports: {e}").red()));
    } else {
        empty.push(Line::from("  Nothing listening right now."));
        empty.push(Line::from(
            "  Start a dev server with s on the Projects tab.".fg(DIM),
        ));
        if !app.show_all_ports {
            empty.push(Line::from("  Press a to include system services.".fg(DIM)));
        }
    }
    render_table(
        f,
        app,
        area,
        TableSpec {
            title: if app.show_all_ports {
                "Ports · all"
            } else {
                "Ports · dev servers"
            },
            header: vec![
                "Port", "Process", "PID", "Project", "Up", "Mem", "CPU", "Bind",
            ],
            widths: vec![
                Constraint::Length(7),
                Constraint::Min(10),
                Constraint::Length(7),
                Constraint::Min(10),
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Length(4),
                Constraint::Length(7),
            ],
            rows,
            empty: empty_message(app.loaded.ports, "Looking for listening ports…", empty),
        },
    );
}

fn port_details(app: &App, l: &Listener) -> Vec<Line<'static>> {
    let mut out = vec![
        Line::from(vec![
            Span::styled(format!(":{} ", l.port), Style::new().fg(ACCENT).bold()),
            Span::styled(l.process.clone(), Style::new().bold()),
        ]),
        Line::from(Span::styled(l.url(), Style::new().fg(ACCENT).underlined())),
        Line::from(""),
        Line::from(vec!["pid      ".fg(DIM), Span::raw(l.pid.to_string())]),
        Line::from(vec![
            "project  ".fg(DIM),
            Span::raw(l.project.clone().unwrap_or_else(|| "-".into())),
        ]),
        Line::from(vec![
            "cwd      ".fg(DIM),
            Span::raw(
                l.cwd
                    .as_deref()
                    .map(util::tilde)
                    .unwrap_or_else(|| "-".into()),
            ),
        ]),
        Line::from(vec![
            "started  ".fg(DIM),
            Span::raw(
                l.started
                    .map(|t| format!("{} ago", util::ago(t)))
                    .unwrap_or_else(|| "-".into()),
            ),
        ]),
        Line::from(vec![
            "memory   ".fg(DIM),
            Span::raw(if l.memory > 0 {
                util::bytes(l.memory)
            } else {
                "-".into()
            }),
        ]),
        Line::from(vec!["bind     ".fg(DIM), Span::raw(l.addrs.join(", "))]),
    ];
    if let Some(r) = l.project.as_ref().and_then(|p| app.run_for(p)) {
        out.push(Line::from(vec![
            "         ".into(),
            format!("◆ started by hangar (group {})", r.pid).green(),
        ]));
    }
    out.push(Line::from(""));
    out.push(Line::from("command".fg(DIM)));
    out.push(Line::from(l.command.clone()));
    out
}

// ---------- Deploys ----------

fn deploys_table(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .visible_deploys()
        .into_iter()
        .map(|d| {
            Row::new(vec![
                Cell::from(Span::styled(
                    format!("{} {}", d.state.symbol(), d.state.label()),
                    Style::new().fg(state_color(d.state)),
                )),
                Cell::from(d.project.clone()),
                Cell::from(Span::styled(
                    match d.source {
                        Source::Vercel => "▲",
                        Source::GitHub => "⚙",
                    },
                    Style::new().fg(DIM),
                )),
                Cell::from(Span::styled(
                    d.detail.clone().unwrap_or_default(),
                    Style::new().fg(DIM),
                )),
                Cell::from(d.title.clone()),
                Cell::from(Span::styled(util::ago(d.created), Style::new().fg(DIM))),
                Cell::from(Span::styled(
                    d.duration().map(util::duration).unwrap_or_default(),
                    Style::new().fg(DIM),
                )),
            ])
        })
        .collect();
    let mut empty = vec![
        Line::from(""),
        Line::from("  No deployments or CI runs found."),
    ];
    empty.extend(source_summary(app));
    render_table(
        f,
        app,
        area,
        TableSpec {
            title: "Deploys",
            header: vec!["State", "Project", "", "Target", "Title", "Age", "Took"],
            widths: vec![
                Constraint::Length(10),
                Constraint::Max(22),
                Constraint::Length(1),
                Constraint::Max(14),
                Constraint::Min(16),
                Constraint::Length(4),
                Constraint::Length(7),
            ],
            rows,
            empty: empty_message(app.loaded.deploys, "Fetching deployments…", empty),
        },
    );
}

fn source_summary(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from("")];
    for (name, status) in [
        ("▲ Vercel", &app.deploys.vercel),
        ("⚙ GitHub", &app.deploys.github),
    ] {
        let (text, color) = match status {
            SourceStatus::Ok => ("connected".to_string(), Color::Green),
            SourceStatus::Off(m) => (m.clone(), DIM),
            SourceStatus::Error(e) => (e.clone(), Color::Red),
        };
        out.push(Line::from(vec![
            Span::styled(format!("  {name:<9}"), Style::new().fg(DIM)),
            Span::styled(text, Style::new().fg(color)),
        ]));
    }
    out
}

fn deploy_details(d: &Deploy) -> Vec<Line<'static>> {
    let mut out = vec![
        Line::from(Span::styled(
            format!("{} {}", d.state.symbol(), d.state.label()),
            Style::new().fg(state_color(d.state)).bold(),
        )),
        Line::from(Span::styled(d.title.clone(), Style::new().bold())),
        Line::from(""),
        Line::from(vec!["project  ".fg(DIM), Span::raw(d.project.clone())]),
        Line::from(vec!["source   ".fg(DIM), Span::raw(d.source.label())]),
    ];
    if let Some(x) = &d.detail {
        out.push(Line::from(vec![
            if d.source == Source::Vercel {
                "target   "
            } else {
                "workflow "
            }
            .fg(DIM),
            Span::raw(x.clone()),
        ]));
    }
    if d.branch.is_some() || d.sha.is_some() {
        let commit = format!(
            "{}{}",
            d.branch.clone().unwrap_or_default(),
            d.sha
                .as_ref()
                .map(|s| format!(" @ {s}"))
                .unwrap_or_default()
        );
        out.push(Line::from(vec!["commit   ".fg(DIM), Span::raw(commit)]));
    }
    if let Some(a) = &d.actor {
        out.push(Line::from(vec!["by       ".fg(DIM), Span::raw(a.clone())]));
    }
    let when = chrono::DateTime::from_timestamp(d.created, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%b %-d %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    out.push(Line::from(vec![
        "started  ".fg(DIM),
        Span::raw(format!("{when} ({} ago)", util::ago(d.created))),
    ]));
    if let Some(dur) = d.duration() {
        out.push(Line::from(vec![
            if d.state.active() {
                "running  "
            } else {
                "took     "
            }
            .fg(DIM),
            Span::raw(util::duration(dur)),
        ]));
    }
    out.push(Line::from(""));
    if let Some(u) = &d.url {
        out.push(Line::from(Span::styled(
            u.clone(),
            Style::new().fg(ACCENT).underlined(),
        )));
    }
    if let Some(u) = &d.inspect_url {
        out.push(Line::from(vec![
            "inspect  ".fg(DIM),
            Span::styled(u.clone(), Style::new().fg(ACCENT)),
        ]));
    }
    out
}

// ---------- Footer + popups ----------

fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    if app.editing_filter {
        return vec![("enter", "keep"), ("esc", "clear"), ("↑↓", "move")];
    }
    let mut h = match app.tab {
        Tab::Projects => vec![
            ("s", "start/stop"),
            ("o", "edit"),
            ("l", "logs"),
            ("u", "open app"),
            ("b", "repo"),
            ("D", "deploy"),
        ],
        Tab::Ports => vec![
            ("x", "kill"),
            ("X", "force"),
            ("o", "open"),
            ("p", "project"),
            ("a", "all"),
        ],
        Tab::Deploys => vec![
            ("o", "open"),
            ("i", "inspect"),
            ("p", "project"),
            ("c", "copy"),
        ],
    };
    h.extend([
        ("/", "filter"),
        ("r", "refresh"),
        ("?", "help"),
        ("q", "quit"),
    ]);
    h
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if app.editing_filter {
        let line = Line::from(vec![
            Span::styled(
                " / ",
                Style::new().fg(Color::Black).bg(Color::Yellow).bold(),
            ),
            Span::raw(format!(" {}", app.filters[app.tab.index()])),
            Span::styled("▏", Style::new().fg(Color::Yellow)),
        ]);
        f.render_widget(line, area);
        return;
    }
    if let Some(s) = &app.status {
        let (color, icon) = match s.level {
            Level::Ok => (Color::Green, "✓"),
            Level::Err => (Color::Red, "✗"),
            Level::Info => (ACCENT, "…"),
        };
        let line = Line::from(vec![
            Span::styled(format!(" {icon} "), Style::new().fg(color).bold()),
            Span::styled(s.text.clone(), Style::new().fg(color)),
        ]);
        f.render_widget(line, area);
        return;
    }
    let mut spans = vec![Span::raw(" ")];
    for (k, label) in hints(app) {
        spans.push(Span::styled(k, Style::new().fg(ACCENT).bold()));
        spans.push(Span::styled(format!(" {label}  "), Style::new().fg(DIM)));
    }
    f.render_widget(Line::from(spans), area);
    let age = app.updated[app.tab.index()].map(|t| t.elapsed().as_secs() as i64);
    let label = match (&app.busy.first(), age) {
        (Some(b), _) => format!("{b}… "),
        (None, Some(a)) => format!(
            "updated {} ",
            if a < 5 {
                "now".into()
            } else {
                format!("{} ago", util::age(a))
            }
        ),
        (None, None) => String::new(),
    };
    let right = Line::from(Span::styled(label, Style::new().fg(FAINT))).alignment(Alignment::Right);
    let hint_width: usize = hints(app)
        .iter()
        .map(|(k, l)| k.chars().count() + l.chars().count() + 3)
        .sum::<usize>()
        + 1;
    if (area.width as usize) > hint_width + right.width() {
        f.render_widget(right, area);
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2)).max(1);
    let h = h.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn popup_block(title: &str, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(color).bold(),
        ))
}

fn draw_help(f: &mut Frame, area: Rect) {
    let section = |s: &str| Line::from(Span::styled(s.to_string(), Style::new().fg(ACCENT).bold()));
    let key = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<12}"), Style::new().fg(Color::White).bold()),
            Span::styled(d.to_string(), Style::new().fg(DIM)),
        ])
    };
    let lines = vec![
        section("Everywhere"),
        key("1 2 3 ⇥", "switch tab"),
        key("j k ↑ ↓", "move   g/G top/bottom"),
        key("/", "fuzzy filter (esc clears)"),
        key("r / R", "refresh tab / everything"),
        key("q", "quit"),
        Line::from(""),
        section("Projects"),
        key("s", "start / stop the dev server"),
        key("l", "dev server logs"),
        key("o enter", "open in editor"),
        key("u", "open the running app"),
        key("b / v", "open repo / latest deploy"),
        key("f t c", "finder · terminal · copy path"),
        key("D", "deploy to Vercel production"),
        Line::from(""),
        section("Ports"),
        key("x / X", "stop (SIGTERM) / force kill"),
        key("o", "open in browser"),
        key("p", "jump to project"),
        key("a", "show system services"),
        Line::from(""),
        section("Deploys"),
        key("o / i", "open / Vercel inspector"),
        key("p c", "jump to project · copy URL"),
    ];
    let r = centered(area, 50, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(popup_block("Keys — any key closes", ACCENT)),
        r,
    );
}

fn draw_confirm(f: &mut Frame, area: Rect, title: &str, body: &str) {
    let mut text = Text::from(body.to_string());
    text.push_line(Line::from(""));
    text.push_line(Line::from(vec![
        Span::styled(
            " y ",
            Style::new().fg(Color::Black).bg(Color::Yellow).bold(),
        ),
        Span::raw(" confirm   "),
        Span::styled(" n ", Style::new().fg(Color::Black).bg(DIM).bold()),
        Span::raw(" cancel"),
    ]));
    let r = centered(area, 60, 9);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(popup_block(title, Color::Yellow)),
        r,
    );
}

fn draw_logs(
    f: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[String],
    scroll: usize,
    follow: bool,
) {
    let r = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    f.render_widget(Clear, r);
    let inner_h = r.height.saturating_sub(2) as usize;
    // `scroll` is the index of the last visible line.
    let last = if lines.is_empty() {
        0
    } else {
        scroll.min(lines.len() - 1)
    };
    let first = (last + 1).saturating_sub(inner_h);
    let visible: Vec<Line> = lines
        .iter()
        .skip(first)
        .take(inner_h)
        .map(|l| Line::from(l.clone()))
        .collect();
    let state = if follow {
        "following · j/k scroll"
    } else {
        "paused · G to follow"
    };
    let block = popup_block(title, ACCENT).title_bottom(
        Line::from(format!(" {state} · esc close ").fg(DIM)).alignment(Alignment::Right),
    );
    let body = if visible.is_empty() {
        vec![Line::from("(empty log)".fg(DIM))]
    } else {
        visible
    };
    f.render_widget(Paragraph::new(body).block(block), r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::worker::Event;
    use crate::config::Config;
    use crate::deploys::{Report, SourceStatus};
    use crate::projects::{Commit, GitInfo, Kind, Remote};
    use crate::runner::Runner;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;
    use std::sync::mpsc;

    fn fixture_app() -> App {
        let (tx, _rx) = mpsc::channel();
        let state = std::env::temp_dir().join(format!("hangar-ui-test-{}", std::process::id()));
        let mut app = App::new(
            Arc::new(Config::default()),
            tx,
            Arc::new(Runner::new(state)),
        );
        let project = |name: &str, dirty: bool| Project {
            name: name.into(),
            path: format!("/tmp/{name}").into(),
            kind: Kind::Vite,
            dev_command: Some(vec!["npm".into(), "run".into(), "dev".into()]),
            scripts: vec!["dev".into()],
            git: Some(GitInfo {
                branch: Some("main".into()),
                head: Some("abc1234".into()),
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 0,
                staged: 0,
                modified: if dirty { 3 } else { 0 },
                untracked: 0,
                conflicted: 0,
                commits: vec![Commit {
                    sha: "abc1234".into(),
                    subject:
                        "A very long commit subject that should be truncated somewhere sensible"
                            .into(),
                    author: "me".into(),
                    time: util::now_unix() - 300,
                }],
                remote: Some(Remote {
                    url: "x".into(),
                    host: "github.com".into(),
                    owner: "me".into(),
                    repo: name.into(),
                }),
            }),
            git_error: None,
            vercel: None,
        };
        app.on_event(Event::Projects(vec![
            project("wayfarer", true),
            project("pocket-golf", false),
            Project {
                git: None,
                git_error: Some("bad".into()),
                ..project("broken", false)
            },
        ]));
        app.on_event(Event::Ports(Ok(vec![Listener {
            port: 5173,
            addrs: vec!["127.0.0.1".into()],
            pid: 4242,
            process: "node".into(),
            command: "node vite --host".into(),
            cwd: Some("/tmp/wayfarer".into()),
            memory: 90_000_000,
            cpu: 2.0,
            started: Some(util::now_unix() - 60),
            mine: true,
            project: Some("wayfarer".into()),
            system: false,
        }])));
        let json: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/vercel_deployments.json"))
                .unwrap();
        let mut deploys = crate::deploys::vercel::parse(&json, "wayfarer");
        let gh: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/github_runs.json")).unwrap();
        deploys.extend(crate::deploys::github::parse(&gh, "pocket-golf"));
        app.on_event(Event::Deploys(Report {
            deploys,
            vercel: SourceStatus::Ok,
            github: SourceStatus::Error("rate limited".into()),
            fetched_at: 0,
        }));
        app
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect::<String>()
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn renders_every_tab_at_every_size_without_panicking() {
        let mut app = fixture_app();
        for (w, h) in [
            (1, 1),
            (20, 5),
            (30, 6),
            (40, 10),
            (80, 24),
            (100, 30),
            (160, 50),
            (300, 100),
        ] {
            for tab in Tab::ALL {
                app.tab = tab;
                render(&mut app, w, h);
            }
            for popup in [
                Popup::Help,
                Popup::Confirm {
                    title: "Kill?".into(),
                    body: "x".repeat(500),
                    action: super::super::Action::StopServer {
                        project: "x".into(),
                    },
                },
                Popup::Logs {
                    title: "logs".into(),
                    path: "/nope".into(),
                    lines: (0..500).map(|i| format!("line {i}")).collect(),
                    scroll: usize::MAX,
                    follow: true,
                    read_at: Some(std::time::Instant::now()),
                },
                Popup::Logs {
                    title: "logs".into(),
                    path: "/nope".into(),
                    lines: vec![],
                    scroll: 3,
                    follow: false,
                    read_at: Some(std::time::Instant::now()),
                },
            ] {
                app.popup = Some(popup);
                render(&mut app, w, h);
            }
            app.popup = None;
        }
    }

    #[test]
    fn shows_the_important_bits() {
        let mut app = fixture_app();
        let screen = render(&mut app, 160, 40);
        assert!(screen.contains("wayfarer") && screen.contains("pocket-golf"));
        assert!(screen.contains("3 changed"));
        assert!(screen.contains("git error"));
        assert!(
            screen.contains("http://localhost:5173"),
            "running server shown in details"
        );
        assert!(screen.contains("1 serving"));

        press(&mut app, KeyCode::Char('2'));
        let screen = render(&mut app, 160, 40);
        assert!(screen.contains(":5173") && screen.contains("node vite --host"));

        press(&mut app, KeyCode::Char('3'));
        let screen = render(&mut app, 160, 40);
        assert!(screen.contains("Add bosses"));
        assert!(screen.contains("building"));
    }

    #[test]
    fn filter_navigation_and_selection_stability() {
        let mut app = fixture_app();
        press(&mut app, KeyCode::Char('/'));
        for c in "pkt".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.visible_projects().len(), 1);
        assert_eq!(app.selected_project().unwrap().name, "pocket-golf");
        press(&mut app, KeyCode::Enter);
        assert!(!app.editing_filter);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.visible_projects().len(), 3);

        // Selection follows the item when the list reorders.
        app.select(Tab::Projects, 1);
        let chosen = app.selected_project().unwrap().name.clone();
        let mut reordered = app.projects.clone();
        reordered.reverse();
        app.on_event(Event::Projects(reordered));
        assert_eq!(app.selected_project().unwrap().name, chosen);

        // Selection clamps when the list shrinks.
        app.select(Tab::Projects, 99);
        assert_eq!(app.selected[0], 2);
        app.on_event(Event::Projects(vec![]));
        assert_eq!(app.selected[0], 0);
        assert!(app.selected_project().is_none());
        press(&mut app, KeyCode::Char('s')); // no-op with nothing selected
        render(&mut app, 80, 24);
    }

    #[test]
    fn long_text_is_truncated_not_wrapped() {
        let mut app = fixture_app();
        let mut projects = app.projects.clone();
        if let Some(g) = projects[0].git.as_mut() {
            g.untracked = 10_083;
        }
        app.on_event(Event::Projects(projects));
        let screen = render(&mut app, 160, 40);
        assert!(screen.contains("999+ changed"));
        assert!(screen.contains("A very long commit subject"));
        assert!(
            screen.contains('…'),
            "the subject was shortened to fit the details pane"
        );
    }

    #[test]
    fn kill_requires_confirmation() {
        let mut app = fixture_app();
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(app.popup, Some(Popup::Confirm { .. })));
        press(&mut app, KeyCode::Char('n'));
        assert!(app.popup.is_none());
        assert!(app.busy.is_empty(), "cancelled: nothing was killed");
        press(&mut app, KeyCode::Char('z'));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn hides_system_ports_until_toggled() {
        let mut app = fixture_app();
        let mut ports = app.ports.clone();
        ports.push(Listener {
            port: 7000,
            process: "ControlCenter".into(),
            project: None,
            system: true,
            pid: 9,
            ..ports[0].clone()
        });
        app.on_event(Event::Ports(Ok(ports)));
        assert_eq!(app.visible_ports().len(), 1);
        app.tab = Tab::Ports;
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.visible_ports().len(), 2);
        app.on_event(Event::Ports(Err("lsof missing".into())));
        assert_eq!(app.ports.len(), 2, "last good data is kept on error");
    }
}
