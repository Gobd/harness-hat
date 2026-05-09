use super::*;

pub(crate) fn render_exec_approval_overlay(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    item_idx: usize,
) {
    let Some(item) = app.pending_exec.get(item_idx) else {
        return;
    };

    let popup_area = centered_rect(72, 56, 12, area);
    frame.render_widget(Clear, popup_area);

    let match_str = match &item.matched_command {
        Some(name) => format!("rule: {name}"),
        None => "unlisted command".to_string(),
    };

    let action_line = Line::from(vec![
        Span::styled(
            "[y/↵] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Approve  ", Style::default().fg(Color::White)),
        Span::styled(
            "[r] ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Always allow  ", Style::default().fg(Color::White)),
        Span::styled(
            "[n/Esc] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Deny  ", Style::default().fg(Color::White)),
        Span::styled(
            "[d] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Always deny", Style::default().fg(Color::White)),
    ]);

    let queue_total = app
        .pending_exec
        .iter()
        .filter(|i| i.project == item.project)
        .count();
    let queue_pos = app
        .pending_exec
        .iter()
        .filter(|i| i.project == item.project)
        .position(|i| i.id == item.id)
        .map(|i| i + 1)
        .unwrap_or(1);
    let source_container = item
        .container_id
        .clone()
        .unwrap_or_else(|| "unknown-container".to_string());
    let command_label = match &item.image {
        Some(image) => format!("--image {image} {}", item.argv.join(" ")),
        None => item.argv.join(" "),
    };
    let command_label = if item.timeout_secs == crate::rules::DEFAULT_TIMEOUT_SECS {
        command_label
    } else {
        format!("--timeout {} {}", item.timeout_secs, command_label)
    };

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  APPROVAL REQUIRED",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Command : ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                command_label,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Workspace: ", Style::default().fg(Color::DarkGray)),
            Span::styled(item.project.clone(), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Source  : ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("workspace={}  container={}", item.project, source_container),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Queue   : ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{}/{} for workspace '{}' (exec total: {}, net total: {})",
                    queue_pos,
                    queue_total.max(1),
                    item.project,
                    app.pending_exec.len(),
                    app.pending_net.len()
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Host cwd: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                item.cwd.display().to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Match   : ", Style::default().fg(Color::DarkGray)),
            Span::styled(match_str, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
        action_line,
        Line::from(""),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Exec Approval Required ")
                .title_alignment(Alignment::Center)
                .title_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup_area,
    );
}

// ── Network approval overlay ──────────────────────────────────────────────────

pub(crate) fn render_net_approval_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let Some(item) = app.pending_net.first() else {
        return;
    };

    let show_proxy_details = item.source_status != "listener_bound_source";
    let popup_area = centered_rect(86, 60, if show_proxy_details { 14 } else { 13 }, area);
    frame.render_widget(Clear, popup_area);
    let source_header = pending_network_source_header(app, item);

    let action_line = Line::from(vec![
        Span::styled(
            "[y/↵] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Allow  ", Style::default().fg(Color::White)),
        Span::styled(
            "[r] ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Always allow  ", Style::default().fg(Color::White)),
        Span::styled(
            "[n/Esc] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Deny  ", Style::default().fg(Color::White)),
        Span::styled(
            "[d] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Always deny", Style::default().fg(Color::White)),
    ]);

    let queue_total = app.pending_net.len();
    let merged_total = crate::tui::app::approvals::pending_network_request_count(item);
    let source_workspace = item
        .source_project
        .clone()
        .unwrap_or_else(|| "unknown-workspace".to_string());
    let source_container = item
        .source_container
        .clone()
        .unwrap_or_else(|| "unknown-container".to_string());

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  NETWORK REQUEST",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  {source_header}"),
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Method  : ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                item.method.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Host    : ", Style::default().fg(Color::DarkGray)),
            Span::styled(item.host.clone(), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Path    : ", Style::default().fg(Color::DarkGray)),
            Span::styled(item.path.clone(), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::styled("  Source  : ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "workspace={}  agent={}  docker_container={}",
                    source_workspace,
                    source_container,
                    pending_network_docker_container_id(app, item)
                ),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Queue   : ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "1/{} (merged requests: {}, exec total: {}, net modals: {})",
                    queue_total.max(1),
                    merged_total,
                    app.pending_exec.len(),
                    app.pending_net.len()
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        action_line,
        Line::from(""),
    ];
    if show_proxy_details {
        lines.insert(
            7,
            Line::from(vec![
                Span::styled("  Proxy   : ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "source_status={}  proxy_auth={}",
                        item.source_status, item.has_proxy_authorization
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
        );
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(truncate_middle(
                    &format!(" Network: {source_header} "),
                    popup_area.width.saturating_sub(2) as usize,
                ))
                .title_alignment(Alignment::Center)
                .title_style(
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta)),
        ),
        popup_area,
    );
}

fn pending_network_source_header(app: &App, item: &crate::proxy::PendingNetworkItem) -> String {
    let session = pending_network_source_session(app, item);
    let agent = session
        .map(|session| session.container_name.clone())
        .or_else(|| item.source_container.clone())
        .unwrap_or_else(|| "unknown-agent".to_string());
    let workspace = session
        .map(|session| session.project.clone())
        .or_else(|| item.source_project.clone())
        .unwrap_or_else(|| "unknown-workspace".to_string());
    let docker_container = session
        .map(|session| short_container_id(&session.container_id))
        .unwrap_or_else(|| "unknown".to_string());

    format!("agent={agent}  workspace={workspace}  docker={docker_container}")
}

fn pending_network_docker_container_id(
    app: &App,
    item: &crate::proxy::PendingNetworkItem,
) -> String {
    pending_network_source_session(app, item)
        .map(|session| short_container_id(&session.container_id))
        .unwrap_or_else(|| "unknown".to_string())
}

fn pending_network_source_session<'a>(
    app: &'a App,
    item: &crate::proxy::PendingNetworkItem,
) -> Option<&'a crate::container::ContainerSession> {
    let source_project = item.source_project.as_deref();
    let source_container = item.source_container.as_deref();

    if let Some(source_container) = source_container {
        if let Some(session) = app.sessions.iter().find(|session| {
            source_project.is_none_or(|project| session.project == project)
                && App::container_identity_matches(
                    source_container,
                    &session.container_id,
                    &session.container_name,
                    &session.docker_name,
                )
        }) {
            return Some(session);
        }
    }

    let source_project = source_project?;
    let mut matching_sessions = app
        .sessions
        .iter()
        .filter(|session| session.project == source_project);
    let only = matching_sessions.next()?;
    matching_sessions.next().is_none().then_some(only)
}

pub(crate) fn render_remove_workspace_confirm_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.remove_workspace_confirm.as_ref() else {
        return;
    };
    let popup_area = centered_rect(74, 56, 11, area);
    frame.render_widget(Clear, popup_area);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  REMOVE WORKSPACE",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Workspace: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                state.workspace_name.clone(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  This will stop running containers in this workspace",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "  and remove it from harness-hat.toml.",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "[y/↵] ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Remove  ", Style::default().fg(Color::White)),
            Span::styled(
                "[n/Esc] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Cancel", Style::default().fg(Color::White)),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Confirm Workspace Removal ")
                .title_alignment(Alignment::Center)
                .title_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        ),
        popup_area,
    );
}

pub(crate) fn render_base_rules_changed_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.base_rules_changed.as_ref() else {
        return;
    };
    let popup_area = centered_rect(82, 62, 14, area);
    frame.render_widget(Clear, popup_area);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  SECURITY ALERT",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Rules file changed while Harness Hat is running.",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "  Change detected from an external process (not this CLI).",
            Style::default().fg(Color::White),
        )),
        Line::from(vec![
            Span::styled("  File: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                state.path.display().to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  If you just updated this file or asked an agent to update it, you can ignore this alert.",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Review this file before approving further requests.",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "[Enter/Esc] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Dismiss", Style::default().fg(Color::White)),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Rules File Changed ")
                .title_alignment(Alignment::Center)
                .title_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        ),
        popup_area,
    );
}

// ── Fullscreen log ────────────────────────────────────────────────────────────

pub(crate) fn render_log_fullscreen(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .title(" Log (fullscreen) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));

    let lines: Vec<Line> = app
        .log
        .iter()
        .map(|entry| match entry {
            LogEntry::Audit(e) => {
                let ts = e.timestamp.format("%H:%M:%S").to_string();
                let decision_color = match e.decision {
                    crate::state::DecisionKind::Auto => Color::Green,
                    crate::state::DecisionKind::Approved
                    | crate::state::DecisionKind::Remembered => Color::Cyan,
                    crate::state::DecisionKind::Denied
                    | crate::state::DecisionKind::DeniedByPolicy
                    | crate::state::DecisionKind::TimedOut => Color::Red,
                };
                let exit_str = match e.exit_code {
                    Some(c) => format!(" exit={c}"),
                    None => String::new(),
                };
                Line::from(vec![
                    Span::styled(format!("[{ts}] "), Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{:<6} ", e.decision.as_str()),
                        Style::default()
                            .fg(decision_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<16} ", e.project),
                        Style::default().fg(Color::White),
                    ),
                    Span::raw(e.argv.join(" ")),
                    Span::styled(exit_str, Style::default().fg(Color::DarkGray)),
                ])
            }
            LogEntry::Msg {
                text,
                is_error,
                timestamp,
            } => {
                let ts = timestamp.format("%H:%M:%S").to_string();
                let (prefix, color) = if *is_error {
                    ("ERR   ", Color::Red)
                } else {
                    ("INFO  ", Color::Green)
                };
                Line::from(vec![
                    Span::styled(format!("[{ts}] "), Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{prefix:<6} "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(text.clone(), Style::default().fg(Color::White)),
                ])
            }
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.log_scroll as u16, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub(crate) fn render_status_bar_log(frame: &mut Frame, _app: &mut App, area: Rect) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓/jk]scroll  [o/Esc/q]close",
            Style::default().fg(Color::DarkGray),
        )),
        area,
    );
}

// ── Layout helpers ────────────────────────────────────────────────────────────

pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, min_height: u16, r: Rect) -> Rect {
    let height = ((r.height * percent_y) / 100).max(min_height).min(r.height);
    let width = (r.width * percent_x) / 100;
    Rect {
        x: (r.width.saturating_sub(width)) / 2 + r.x,
        y: (r.height.saturating_sub(height)) / 2 + r.y,
        width,
        height,
    }
}
