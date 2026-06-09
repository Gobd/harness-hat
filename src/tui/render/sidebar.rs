use super::*;

const NETWORK_PAYLOAD_PREVIEW_LINES: usize = 8;
const NETWORK_RECENT_HISTORY_LINES: usize = 6;

pub(crate) fn render_right_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.focus == Focus::Sidebar {
        let selected = app.sidebar_items().get(app.sidebar_idx).cloned();
        match selected {
            Some(SidebarItem::Session(si)) => {
                if let Some(session_idx) = app.first_terminal_for_session_group(si) {
                    render_session_detail(frame, app, area, session_idx, true);
                } else {
                    render_idle(frame, area);
                }
            }
            Some(SidebarItem::SessionTerminal(group_idx, session_pos)) => {
                if let Some(session_idx) = app.session_group_terminal(group_idx, session_pos) {
                    render_session_detail(frame, app, area, session_idx, true);
                } else {
                    render_idle(frame, area);
                }
            }
            Some(SidebarItem::Activity(id)) => {
                render_activity_detail(frame, app, area, id.as_str(), true);
            }
            Some(SidebarItem::NetworkGroup(si)) => {
                render_network_group_detail(frame, app, area, si, true);
            }
            Some(SidebarItem::Settings(pi)) => {
                render_project_settings(frame, app, area, pi, true);
            }
            Some(SidebarItem::Launch(_)) => {
                render_idle(frame, area);
            }
            Some(SidebarItem::Build(_))
                if (app.build_is_running() || app.build_finished.is_some())
                    && build_output_is_selected(app) =>
            {
                render_build_output(frame, app, area, true);
            }
            Some(SidebarItem::NewWorkspace) => {
                if app.new_project.is_some() {
                    render_new_project(frame, app, area, true);
                } else {
                    render_new_project_preview(frame, app, area, true);
                }
            }
            _ => render_idle(frame, area),
        }
        return;
    }

    if app.focus == Focus::Settings {
        let pi = app
            .active_settings_project
            .or_else(|| app.selected_project_idx())
            .unwrap_or(0);
        render_project_settings(frame, app, area, pi, false);
        return;
    }

    if app.focus == Focus::ContainerPicker {
        render_container_picker(frame, app, area, false);
        return;
    }

    if app.focus == Focus::Activity {
        if let Some(id) = app.active_activity.clone() {
            render_activity_detail(frame, app, area, id.as_str(), false);
        } else {
            render_idle(frame, area);
        }
        return;
    }

    if app.focus == Focus::Network {
        if let Some(si) = app.active_network_session {
            render_network_group_detail(frame, app, area, si, false);
        } else {
            render_idle(frame, area);
        }
        return;
    }

    if app.focus == Focus::ImageBuild {
        if app.build_is_running() || app.build_finished.is_some() {
            render_build_output(frame, app, area, false);
        } else {
            render_image_build(frame, app, area, false);
        }
        return;
    }

    if app.build_is_running() && build_output_is_selected(app) {
        render_build_output(frame, app, area, false);
        return;
    }

    if app.focus == Focus::NewWorkspace {
        render_new_project(frame, app, area, false);
        return;
    }

    let has_modal = app.has_pending_approval_modal();

    match app.active_session {
        Some(si) if si < app.sessions.len() => {
            render_session_detail(frame, app, area, si, has_modal);
        }
        _ => render_idle(frame, area),
    }
}

pub(crate) fn render_terminal_overlays(frame: &mut Frame, app: &mut App, area: Rect) {
    if !app.pending_net.is_empty() {
        render_net_approval_overlay(frame, app, area);
    }
}

pub(crate) fn build_output_is_selected(app: &App) -> bool {
    matches!(
        app.sidebar_items().get(app.sidebar_idx),
        Some(SidebarItem::Build(_))
    )
}

pub(crate) fn render_activity_detail(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    activity_id: &str,
    dimmed: bool,
) {
    let Some(activity) = app.activity_by_id(activity_id) else {
        render_idle(frame, area);
        return;
    };
    let tone = |c| maybe_dim(c, dimmed);
    let title = " Network Activity ";
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(tone(Color::Cyan))
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tone(Color::Cyan)));

    let _status_color = activity_status_color(&activity.state);
    let activity_focused = !dimmed && app.focus == Focus::Activity;
    let in_scroll_mode = activity_focused && app.scroll_mode;
    let mut lines = Vec::new();
    lines.push(Line::from(""));
    append_activity_summary_lines(&mut lines, activity, &tone);

    append_network_detail_lines(&mut lines, activity, &tone);

    append_activity_status_line(&mut lines, activity, &tone);

    lines.push(Line::from(""));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let footer_height = if inner.height > 0 { 1 } else { 0 };
    let header_height = (lines.len() as u16).min(inner.height.saturating_sub(footer_height));
    let footer_height = footer_height.min(inner.height.saturating_sub(header_height));
    let output_height = inner
        .height
        .saturating_sub(header_height)
        .saturating_sub(footer_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(output_height),
            Constraint::Length(footer_height),
        ])
        .split(inner);

    if header_height > 0 {
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[0]);
    }
    if output_height > 0 {
        activity.terminal.resize(chunks[1].height, chunks[1].width);
        let mut term = activity.terminal.term.lock();
        render_term_buffer(
            frame,
            chunks[1],
            &mut *term,
            dimmed,
            false,
            in_scroll_mode,
            app.terminal_scroll,
        );
    }
    if footer_height > 0 {
        let footer = if in_scroll_mode {
            "  SCROLL: [↑↓/jk]line  [PgUp/PgDn]page  [g/G]top/bottom  [Esc/q]exit scroll"
        } else {
            "  [^C/^Q] Quit   [^S] Scroll   [Esc/^B] Back to sidebar"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                footer,
                Style::default().fg(tone(Color::DarkGray)),
            ))),
            chunks[2],
        );
    }
}

pub(crate) fn render_network_group_detail(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    session_idx: usize,
    dimmed: bool,
) {
    let activities = app.network_activities_for_session(session_idx);
    let tone = |c| maybe_dim(c, dimmed);
    let count = activities.len();
    let title = network_group_title(app, session_idx, count, area.width);
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(tone(Color::Magenta))
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tone(Color::Cyan)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if activities.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No retained network requests.",
                Style::default().fg(tone(Color::DarkGray)),
            ))),
            inner,
        );
        return;
    }

    let cursor = app.network_cursor_for_len(session_idx, activities.len());
    let focused = app.focus == Focus::Network && app.active_network_session == Some(session_idx);
    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  #   State      Method  ",
            Style::default().fg(tone(Color::DarkGray)),
        ),
        Span::styled("Request", Style::default().fg(tone(Color::DarkGray))),
    ]));

    let max_request_rows = (inner.height as usize)
        .saturating_sub(16)
        .max(3)
        .min(activities.len());
    let start = if activities.len() <= max_request_rows {
        0
    } else {
        cursor
            .saturating_sub(max_request_rows / 2)
            .min(activities.len().saturating_sub(max_request_rows))
    };
    let end = (start + max_request_rows).min(activities.len());
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("      ... {start} earlier request(s)"),
            Style::default().fg(tone(Color::DarkGray)),
        )));
    }

    for (idx, activity) in activities.iter().enumerate().skip(start).take(end - start) {
        let selected = idx == cursor;
        let marker = if selected && focused { "▶" } else { " " };
        let status_color = activity_status_color(&activity.state);
        let request = match &activity.kind {
            crate::activity::ActivityKind::Network {
                method, host, path, ..
            } => {
                let target = truncate_middle(&format!("{host}{path}"), 58);
                format!("{method:<7} {target}")
            }
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {marker}{:>2} ", idx + 1),
                Style::default().fg(tone(if selected {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })),
            ),
            Span::styled(
                format!("{:<10}", activity.state.label()),
                Style::default()
                    .fg(tone(status_color))
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(
                request,
                Style::default().fg(tone(if selected { Color::White } else { Color::Gray })),
            ),
        ]));
    }
    if end < activities.len() {
        lines.push(Line::from(Span::styled(
            format!("      ... {} later request(s)", activities.len() - end),
            Style::default().fg(tone(Color::DarkGray)),
        )));
    }

    if let Some(activity) = activities.get(cursor) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Selected",
            Style::default()
                .fg(tone(Color::Magenta))
                .add_modifier(Modifier::BOLD),
        )));
        append_activity_summary_lines(&mut lines, activity, &tone);
        append_network_detail_lines(&mut lines, activity, &tone);
        append_activity_status_line(&mut lines, activity, &tone);
        append_recent_activity_history_lines(&mut lines, activity, &tone);
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if focused {
            "  [↑↓/jk] Select request   [^C/^Q] Quit   [Esc/^B] Back to sidebar"
        } else {
            "  [↵/l] Open network list   [Esc/^B] Back to sidebar"
        },
        Style::default().fg(tone(Color::DarkGray)),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn network_group_title(app: &App, session_idx: usize, count: usize, width: u16) -> String {
    let raw = app
        .sessions
        .get(session_idx)
        .map(|session| {
            format!(
                " Network: container={} workspace={} docker={} ({count}) ",
                session.container_name,
                session.project,
                short_container_id(&session.container_id)
            )
        })
        .unwrap_or_else(|| format!(" Network ({count}) "));
    truncate_middle(&raw, width.saturating_sub(2) as usize)
}

fn append_activity_summary_lines(
    lines: &mut Vec<Line>,
    activity: &crate::activity::Activity,
    tone: &impl Fn(Color) -> Color,
) {
    lines.push(Line::from(vec![
        Span::styled("  State   : ", Style::default().fg(tone(Color::DarkGray))),
        Span::styled(
            activity.state.label(),
            Style::default()
                .fg(tone(activity_status_color(&activity.state)))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  total: {}s", activity.elapsed_duration().as_secs()),
            Style::default().fg(tone(Color::DarkGray)),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Source  : ", Style::default().fg(tone(Color::DarkGray))),
        Span::styled(
            format!(
                "workspace={}  container={}",
                activity.project,
                activity.container.as_deref().unwrap_or("<unknown>")
            ),
            Style::default().fg(tone(Color::White)),
        ),
    ]));
}

fn append_network_detail_lines(
    lines: &mut Vec<Line>,
    activity: &crate::activity::Activity,
    tone: &impl Fn(Color) -> Color,
) {
    let crate::activity::ActivityKind::Network {
        method,
        host,
        path,
        protocol,
        payload_preview,
        payload_truncated,
        content_type,
        content_length,
    } = &activity.kind;

    lines.push(Line::from(vec![
        Span::styled("  Method  : ", Style::default().fg(tone(Color::DarkGray))),
        Span::styled(method.clone(), Style::default().fg(tone(Color::White))),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Domain  : ", Style::default().fg(tone(Color::DarkGray))),
        Span::styled(host.clone(), Style::default().fg(tone(Color::White))),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Path    : ", Style::default().fg(tone(Color::DarkGray))),
        Span::styled(path.clone(), Style::default().fg(tone(Color::White))),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Protocol: ", Style::default().fg(tone(Color::DarkGray))),
        Span::styled(protocol.clone(), Style::default().fg(tone(Color::White))),
    ]));
    if let Some(content_type) = content_type {
        lines.push(Line::from(vec![
            Span::styled("  Type    : ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(
                content_type.clone(),
                Style::default().fg(tone(Color::White)),
            ),
        ]));
    }
    if let Some(content_length) = content_length {
        lines.push(Line::from(vec![
            Span::styled("  Payload : ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(
                format!(
                    "{}{}",
                    crate::proxy::format_byte_count(*content_length as u64),
                    if *payload_truncated {
                        " (preview truncated)"
                    } else {
                        ""
                    }
                ),
                Style::default().fg(tone(Color::White)),
            ),
        ]));
    }
    if !payload_preview.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Payload preview",
            Style::default()
                .fg(tone(Color::Magenta))
                .add_modifier(Modifier::BOLD),
        )));
        for line in payload_preview.lines().take(NETWORK_PAYLOAD_PREVIEW_LINES) {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(tone(Color::DarkGray)),
            )));
        }
    }
}

fn append_activity_status_line(
    lines: &mut Vec<Line>,
    activity: &crate::activity::Activity,
    tone: &impl Fn(Color) -> Color,
) {
    let Some(status) = &activity.status else {
        return;
    };
    lines.push(Line::from(vec![
        Span::styled("  Status  : ", Style::default().fg(tone(Color::DarkGray))),
        Span::styled(
            status.clone(),
            Style::default()
                .fg(tone(activity_status_color(&activity.state)))
                .add_modifier(Modifier::BOLD),
        ),
    ]));
}

fn append_recent_activity_history_lines(
    lines: &mut Vec<Line>,
    activity: &crate::activity::Activity,
    tone: &impl Fn(Color) -> Color,
) {
    let history = activity
        .lines
        .iter()
        .rev()
        .take(NETWORK_RECENT_HISTORY_LINES)
        .cloned()
        .collect::<Vec<_>>();
    if history.is_empty() {
        return;
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Recent history",
        Style::default()
            .fg(tone(Color::Cyan))
            .add_modifier(Modifier::BOLD),
    )));
    for line in history.iter().rev() {
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            Style::default().fg(tone(Color::DarkGray)),
        )));
    }
}

fn activity_status_color(state: &crate::activity::ActivityState) -> Color {
    match state {
        crate::activity::ActivityState::PendingApproval => Color::Yellow,
        crate::activity::ActivityState::Running => Color::Cyan,
        crate::activity::ActivityState::Forwarding => Color::Yellow,
        crate::activity::ActivityState::Complete => Color::Green,
        crate::activity::ActivityState::Failed => Color::Red,
        crate::activity::ActivityState::Denied => Color::Red,
        crate::activity::ActivityState::Cancelled => Color::Red,
    }
}

// ── Idle screen ───────────────────────────────────────────────────────────────

pub(crate) fn render_idle(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Select a workspace and press [↵] to launch a container.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Select a running container and press [↵] to attach.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub(crate) fn render_project_settings(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    project_idx: usize,
    dimmed: bool,
) {
    let cfg = app.config.get();
    let Some(proj) = cfg.workspaces.get(project_idx) else {
        render_idle(frame, area);
        return;
    };

    let tone = |c| maybe_dim(c, dimmed);
    let focused = app.focus == Focus::Settings;
    let border_style = if focused {
        Style::default().fg(tone(Color::Cyan))
    } else {
        Style::default().fg(tone(Color::DarkGray))
    };
    let block = Block::default()
        .title(format!(" {} Settings ", proj.name))
        .title_style(
            Style::default()
                .fg(tone(Color::Yellow))
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(border_style);

    let actions = App::settings_action_rows_for();
    let cursor = if actions.is_empty() {
        0
    } else {
        app.settings_cursor.min(actions.len().saturating_sub(1))
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Canonical repo: ",
                Style::default().fg(tone(Color::DarkGray)),
            ),
            Span::styled(
                proj.canonical_path.display().to_string(),
                Style::default().fg(tone(Color::White)),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Actions",
            Style::default()
                .fg(tone(Color::Cyan))
                .add_modifier(Modifier::BOLD),
        )),
    ];

    for (i, action) in actions.iter().enumerate() {
        let selected = focused && i == cursor;
        let marker = if selected { "▶ " } else { "  " };
        let name_style = if selected {
            Style::default()
                .fg(tone(Color::White))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(tone(Color::White))
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {marker}"),
                Style::default().fg(tone(Color::Cyan)),
            ),
            Span::styled(format!("[{}] {}", action.key, action.label), name_style),
        ]));
        lines.push(Line::from(Span::styled(
            format!("      {}", action.desc),
            Style::default().fg(tone(Color::DarkGray)),
        )));
    }

    let rules_path = proj.canonical_path.join("harness-rules.toml");
    let rules_status: Vec<Span> = if !rules_path.exists() {
        vec![
            Span::styled(
                "  harness-rules.toml: ",
                Style::default().fg(tone(Color::DarkGray)),
            ),
            Span::styled("Not Found", Style::default().fg(tone(Color::Yellow))),
        ]
    } else {
        match crate::rules::load(&rules_path) {
            Ok(r) => vec![
                Span::styled(
                    "  harness-rules.toml: ",
                    Style::default().fg(tone(Color::DarkGray)),
                ),
                Span::styled("Loaded", Style::default().fg(tone(Color::Green))),
                Span::styled(
                    format!(
                        "  network: {}",
                        r.network.allowlist.len() + r.network.denylist.len()
                    ),
                    Style::default().fg(tone(Color::White)),
                ),
            ],
            Err(_) => vec![
                Span::styled(
                    "  harness-rules.toml: ",
                    Style::default().fg(tone(Color::DarkGray)),
                ),
                Span::styled("Error", Style::default().fg(tone(Color::Red))),
            ],
        }
    };

    lines.push(Line::from(""));
    lines.push(Line::from(rules_status));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  [^B] Back to sidebar",
        Style::default().fg(tone(Color::DarkGray)),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

// ── Terminal view ─────────────────────────────────────────────────────────────

pub(crate) fn render_terminal_fullscreen_header(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    title_style: Style,
    hint: &str,
) {
    let width = area.width as usize;
    let hint_width = hint.chars().count();
    let line = if width > hint_width + 1 {
        let title = truncate_middle(title, width.saturating_sub(hint_width + 1));
        let title_width = title.chars().count();
        let gap = width.saturating_sub(title_width + hint_width);
        Line::from(vec![
            Span::styled(title, title_style),
            Span::raw(" ".repeat(gap)),
            Span::styled(hint.to_string(), Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(Span::styled(truncate_middle(title, width), title_style))
    };

    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::ActivityState;

    #[test]
    fn activity_status_color_uses_progress_success_failure_palette() {
        assert_eq!(
            activity_status_color(&ActivityState::PendingApproval),
            Color::Yellow
        );
        assert_eq!(activity_status_color(&ActivityState::Running), Color::Cyan);
        assert_eq!(
            activity_status_color(&ActivityState::Forwarding),
            Color::Yellow
        );
        assert_eq!(
            activity_status_color(&ActivityState::Complete),
            Color::Green
        );
        assert_eq!(activity_status_color(&ActivityState::Failed), Color::Red);
        assert_eq!(activity_status_color(&ActivityState::Denied), Color::Red);
        assert_eq!(activity_status_color(&ActivityState::Cancelled), Color::Red);
    }
}
