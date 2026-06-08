use super::*;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};

use super::{App, Focus, SidebarItem};

const LOG_HEIGHT: u16 = 6;
const STATUS_HEIGHT: u16 = 1;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    if app.log_fullscreen {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(STATUS_HEIGHT)])
            .split(area);
        render_log_fullscreen(frame, app, split[0]);
        render_status_bar_log(frame, app, split[1]);
        render_terminal_overlays(frame, app, split[0]);
        if app.base_rules_changed.is_some() {
            render_base_rules_changed_overlay(frame, app, area);
        }
        return;
    }

    if app.terminal_fullscreen {
        if let Some(si) = app.active_session.filter(|&si| si < app.sessions.len()) {
            if app.passthrough_mode {
                render_terminal(frame, app, area, si, app.has_pending_approval_modal(), true);
            } else {
                render_session_detail(frame, app, area, si, app.has_pending_approval_modal());
            }
            render_terminal_overlays(frame, app, area);
            if app.base_rules_changed.is_some() {
                render_base_rules_changed_overlay(frame, app, area);
            } else if app.remove_workspace_confirm.is_some() {
                render_remove_workspace_confirm_overlay(frame, app, area);
            }
        } else {
            render_idle(frame, area);
            render_terminal_overlays(frame, app, area);
            if app.base_rules_changed.is_some() {
                render_base_rules_changed_overlay(frame, app, area);
            } else if app.remove_workspace_confirm.is_some() {
                render_remove_workspace_confirm_overlay(frame, app, area);
            }
        }
        return;
    }

    let show_log_pane = log_pane_height(app) > 0;
    let outer = if show_log_pane {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(LOG_HEIGHT),
                Constraint::Length(STATUS_HEIGHT),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(STATUS_HEIGHT)])
            .split(area)
    };

    let gap_width = right_pane_gap_width(app);
    let main_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(app.config.get().defaults.ui.sidebar_width.max(1)),
            Constraint::Length(gap_width),
            Constraint::Min(0),
        ])
        .split(outer[0]);

    render_sidebar(frame, app, main_row[0]);
    if app.passthrough_mode {
        if let Some(si) = app.active_session.filter(|&si| si < app.sessions.len()) {
            render_terminal(
                frame,
                app,
                main_row[2],
                si,
                app.has_pending_approval_modal(),
                false,
            );
        } else {
            render_idle(frame, main_row[2]);
        }
    } else {
        render_right_pane(frame, app, main_row[2]);
    }
    render_terminal_overlays(frame, app, main_row[2]);
    if show_log_pane {
        render_log(frame, app, outer[1]);
        render_status_bar(frame, app, outer[2]);
    } else {
        render_status_bar(frame, app, outer[1]);
    }
    if app.base_rules_changed.is_some() {
        render_base_rules_changed_overlay(frame, app, area);
    } else if app.remove_workspace_confirm.is_some() {
        render_remove_workspace_confirm_overlay(frame, app, area);
    }
}

pub(crate) fn log_pane_height(app: &App) -> u16 {
    if app.config.get().defaults.ui.show_log_pane {
        LOG_HEIGHT
    } else {
        0
    }
}

pub(crate) fn right_pane_gap_width(app: &App) -> u16 {
    if app.focus == Focus::ImageBuild {
        return 1;
    }

    match app.sidebar_items().get(app.sidebar_idx) {
        Some(SidebarItem::Build(_)) => 1,
        _ => 0,
    }
}

pub fn render_scrollbar(
    frame: &mut Frame,
    track_area: Rect,
    max_scroll: usize,
    current_scroll: usize,
    invert: bool,
) {
    if max_scroll == 0 || track_area.height == 0 {
        return;
    }
    let track_h = track_area.height as usize;
    let total_content = max_scroll + track_h;
    let thumb_size = (track_h * track_h / total_content).max(1);
    let track_range = track_h.saturating_sub(thumb_size);
    let thumb_top = if invert {
        track_range.saturating_sub(current_scroll * track_range / max_scroll)
    } else {
        current_scroll * track_range / max_scroll
    };
    let x = track_area.right().saturating_sub(1);
    for row in 0..track_h {
        let in_thumb = row >= thumb_top && row < thumb_top + thumb_size;
        let (ch, style) = if in_thumb {
            ("┃", Style::default().fg(Color::Yellow))
        } else {
            ("│", Style::default().fg(Color::DarkGray))
        };
        let bar_area = Rect::new(x, track_area.y + row as u16, 1, 1);
        frame.render_widget(Span::styled(ch, style), bar_area);
    }
}

// ── Sidebar ───────────────────────────────────────────────────────────────────

pub(crate) fn render_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .title(" Sessions ")
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);

    let items = app.sidebar_items();
    if !items.is_empty() {
        let visible = area.height.saturating_sub(2).max(1) as usize;
        let selected = app.sidebar_idx.min(items.len().saturating_sub(1));
        let max_offset = items.len().saturating_sub(visible);

        let mut offset = app.sidebar_offset.min(max_offset);
        if selected < offset {
            offset = selected;
        } else if selected >= offset.saturating_add(visible) {
            offset = selected.saturating_add(1).saturating_sub(visible);
        }
        if selected <= 1 {
            // Keep the first row visible when focusing the top of the sidebar.
            offset = 0;
        }
        app.sidebar_offset = offset.min(max_offset);
    } else {
        app.sidebar_offset = 0;
    }
    let cfg = app.config.get();
    let visible = area.height.saturating_sub(2).max(1) as usize;
    let offset = app.sidebar_offset.min(items.len());
    app.refresh_terminal_activity_selection(&items);
    let list_items: Vec<ListItem> = items
        .iter()
        .skip(offset)
        .take(visible)
        .map(|item| match item {
            SidebarItem::Session(si) => {
                let group = app.session_groups.get(*si);
                let session_idx = app.first_terminal_for_session_group(*si);
                let (prefix, name_color, short_id) = if let Some(session_idx) = session_idx {
                    app.sessions.get(session_idx).map_or_else(
                        || ("  ● ".to_string(), Color::DarkGray, String::new()),
                        |session| {
                            let (prefix, name_color) = if session.is_exited() {
                                ("  ✗ ".to_string(), Color::DarkGray)
                            } else if app.session_is_waiting(session_idx) {
                                ("  ? ".to_string(), Color::Yellow)
                            } else {
                                (format!("  {} ", loading_spinner_frame()), Color::Green)
                            };
                            let short_id: String = session.docker_name.chars().take(12).collect();
                            (prefix, name_color, short_id)
                        },
                    )
                } else {
                    ("  ● ".to_string(), Color::DarkGray, String::new())
                };
                let label = group
                    .map(|g| g.workspace_name.as_str())
                    .unwrap_or("<unknown session>");
                let bell = if let Some(session_idx) = session_idx {
                    app.sessions
                        .get(session_idx)
                        .map(|session| session.has_bell())
                        .unwrap_or(false)
                } else {
                    false
                };
                let template = group
                    .map(|g| g.template_name.as_str())
                    .unwrap_or("<unknown template>");
                let mut spans = vec![
                    Span::styled(prefix, Style::default().fg(name_color)),
                    Span::styled(label.to_string(), Style::default().fg(name_color)),
                    Span::styled(
                        format!("  {template}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(format!(" {short_id}"), Style::default().fg(Color::DarkGray)),
                ];
                if bell {
                    spans.push(Span::styled(
                        " [!]",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
            SidebarItem::SessionTerminal(group_idx, session_pos) => {
                let session_idx = app.session_group_terminal(*group_idx, *session_pos);
                if let Some(session_idx) = session_idx {
                    let session = &app.sessions[session_idx];
                    let indent = sidebar_child_indent(app.session_depth(session_idx));
                    let (prefix, name_color) = if session.is_exited() {
                        (format!("{indent}✗"), Color::DarkGray)
                    } else if app.session_is_waiting(session_idx) {
                        (format!("{indent}?"), Color::Yellow)
                    } else {
                        (format!("{indent}{}", loading_spinner_frame()), Color::Green)
                    };
                    let short_id: String = session.docker_name.chars().take(12).collect();
                    let mut spans = vec![
                        Span::styled(prefix, Style::default().fg(name_color)),
                        Span::styled(" ", Style::default().fg(name_color)),
                        Span::styled(
                            session.display_name().to_string(),
                            Style::default().fg(name_color),
                        ),
                        Span::styled(format!(" {short_id}"), Style::default().fg(Color::DarkGray)),
                    ];
                    if session.has_bell() {
                        spans.push(Span::styled(
                            " [!]",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ));
                    }
                    ListItem::new(Line::from(spans))
                } else {
                    let indent = sidebar_child_indent(0);
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{indent}? "), Style::default().fg(Color::Red)),
                        Span::styled("Missing terminal", Style::default().fg(Color::DarkGray)),
                    ]))
                }
            }
            SidebarItem::NetworkGroup(si) => {
                let activities = app.network_activities_for_session(*si);
                let count = activities.len();
                let (marker, color) = network_sidebar_marker(&activities);
                let indent = sidebar_activity_indent_for_session(app, *si);
                ListItem::new(Line::from(vec![
                    Span::styled(indent, Style::default().fg(Color::DarkGray)),
                    Span::styled(marker, Style::default().fg(color)),
                    Span::styled(format!("Network ({count})"), Style::default().fg(color)),
                ]))
            }
            SidebarItem::Activity(id) => {
                if let Some(activity) = app.activity_by_id(id) {
                    let indent = sidebar_activity_indent_for_activity(app, id);
                    let (marker, color) = match &activity.state {
                        crate::activity::ActivityState::PendingApproval => ("? ", Color::Yellow),
                        crate::activity::ActivityState::Running => ("$ ", Color::Cyan),
                        crate::activity::ActivityState::Forwarding => ("⇅ ", Color::Magenta),
                        crate::activity::ActivityState::Complete => ("✓ ", Color::Green),
                        crate::activity::ActivityState::Failed => ("! ", Color::Red),
                        crate::activity::ActivityState::Denied => ("× ", Color::Red),
                        crate::activity::ActivityState::Cancelled => ("× ", Color::DarkGray),
                    };
                    let title = truncate_middle(&activity.title(), 32);
                    ListItem::new(Line::from(vec![
                        Span::styled(indent, Style::default().fg(Color::DarkGray)),
                        Span::styled(marker, Style::default().fg(color)),
                        Span::styled(title, Style::default().fg(color)),
                    ]))
                    .style(activity_sidebar_terminal_style(activity))
                } else {
                    ListItem::new(Line::from(vec![
                        Span::styled("    ? ", Style::default().fg(Color::DarkGray)),
                        Span::styled("unknown activity", Style::default().fg(Color::DarkGray)),
                    ]))
                }
            }
            SidebarItem::Settings(_) => ListItem::new(Line::from(vec![
                Span::styled("  ⚙ ", Style::default().fg(Color::Yellow)),
                Span::styled("Settings", Style::default().fg(Color::DarkGray)),
            ])),
            SidebarItem::WorkspacesHeader => ListItem::new(Line::from(Span::styled(
                "─ Workspaces ─",
                Style::default().fg(Color::DarkGray),
            ))),
            SidebarItem::Launch(pi) => {
                let name = cfg
                    .workspaces
                    .get(*pi)
                    .map(|ws| ws.name.as_str())
                    .unwrap_or("<workspace>");
                let hotkey = app
                    .workspaces
                    .get(*pi)
                    .and_then(|ws| ws.sidebar_hotkey)
                    .map(|h| format!(" [{}]", h.to_ascii_uppercase()))
                    .unwrap_or_default();
                ListItem::new(Line::from(vec![
                    Span::styled("  ▸ ", Style::default().fg(Color::Green)),
                    Span::styled(name.to_string(), Style::default().fg(Color::Gray)),
                    Span::styled(hotkey, Style::default().fg(Color::DarkGray)),
                ]))
            }
            SidebarItem::Build(_) => {
                let image = app
                    .build_container_idx
                    .and_then(|idx| cfg.containers.get(idx))
                    .map(|c| c.image.as_str())
                    .unwrap_or("<unknown>");
                let marker = if app.build_is_running() {
                    loading_spinner_frame()
                } else {
                    "$"
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("  {marker} "), Style::default().fg(Color::Yellow)),
                    Span::styled("docker build", Style::default().fg(Color::Yellow)),
                    Span::styled(format!("  {image}"), Style::default().fg(Color::DarkGray)),
                ]))
            }
            SidebarItem::NewWorkspace => ListItem::new(Line::from(vec![
                Span::styled("+ ", Style::default().fg(Color::Green)),
                Span::styled(
                    "New Workspace...",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ])),
            SidebarItem::NewSession => ListItem::new(Line::from(vec![
                Span::styled("+ ", Style::default().fg(Color::Green)),
                Span::styled(
                    "New Session...",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ])),
        })
        .collect();

    let mut list_state = ListState::default();
    if !items.is_empty() {
        let selected = app.sidebar_idx.min(items.len().saturating_sub(1));
        let rel_selected = selected.saturating_sub(offset);
        list_state.select(Some(rel_selected.min(list_items.len().saturating_sub(1))));
    }

    frame.render_stateful_widget(
        List::new(list_items)
            .block(block)
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol(""),
        area,
        &mut list_state,
    );

    if items.len() > visible && inner.height > 0 {
        let max_offset = items.len().saturating_sub(visible).max(1);
        let offset = app.sidebar_offset.min(max_offset);
        render_scrollbar(frame, inner, max_offset, offset, false);
    }
}

fn sidebar_activity_indent_for_session(app: &App, session_idx: usize) -> String {
    sidebar_child_indent(app.session_depth(session_idx))
}

fn sidebar_activity_indent_for_activity(app: &App, activity_id: &str) -> String {
    app.session_for_activity(activity_id)
        .map(|session_idx| sidebar_activity_indent_for_session(app, session_idx))
        .unwrap_or_else(|| sidebar_child_indent(0))
}

fn sidebar_child_indent(session_depth: usize) -> String {
    "  ".repeat(session_depth + 2)
}

fn network_sidebar_marker(activities: &[&crate::activity::Activity]) -> (&'static str, Color) {
    let mut has_pending = false;
    let mut has_active = false;
    let mut has_failed = false;
    let mut has_cancelled = false;

    for activity in activities {
        match &activity.state {
            crate::activity::ActivityState::PendingApproval => has_pending = true,
            crate::activity::ActivityState::Forwarding
            | crate::activity::ActivityState::Running => has_active = true,
            crate::activity::ActivityState::Failed | crate::activity::ActivityState::Denied => {
                has_failed = true
            }
            crate::activity::ActivityState::Cancelled => has_cancelled = true,
            crate::activity::ActivityState::Complete => {}
        }
    }

    if has_pending {
        ("? ", Color::Yellow)
    } else if has_active {
        ("⇅ ", Color::Magenta)
    } else if has_failed {
        ("! ", Color::Red)
    } else if has_cancelled {
        ("× ", Color::DarkGray)
    } else {
        ("✓ ", Color::Green)
    }
}

fn activity_sidebar_terminal_style(activity: &crate::activity::Activity) -> Style {
    if !activity.state.is_terminal() {
        return Style::default();
    }

    let age_ms = activity
        .terminal_unselected_at
        .map(|unselected_at| unselected_at.elapsed().as_millis())
        .unwrap_or(0);
    let hold_ms = u128::from(crate::activity::ACTIVITY_TERMINAL_HIGHLIGHT_SECS) * 1_000;
    let fade_ms = u128::from(crate::activity::ACTIVITY_TERMINAL_FADE_SECS) * 1_000;
    let level = if age_ms < hold_ms {
        3
    } else if age_ms < hold_ms + fade_ms {
        2usize.saturating_sub((((age_ms - hold_ms) * 3) / fade_ms) as usize)
    } else {
        return Style::default();
    };

    let bg = terminal_activity_background(activity.state.succeeded(), level);
    Style::default().fg(Color::White).bg(bg)
}

fn terminal_activity_background(succeeded: bool, level: usize) -> Color {
    match (succeeded, level) {
        (true, 3) => Color::Rgb(24, 112, 56),
        (true, 2) => Color::Rgb(16, 80, 40),
        (true, 1) => Color::Rgb(10, 48, 28),
        (true, _) => Color::Rgb(6, 28, 18),
        (false, 3) => Color::Rgb(132, 40, 40),
        (false, 2) => Color::Rgb(96, 32, 36),
        (false, 1) => Color::Rgb(56, 24, 28),
        (false, _) => Color::Rgb(32, 18, 20),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sidebar_child_indent_tracks_session_depth() {
        assert_eq!(super::sidebar_child_indent(0), "    ");
        assert_eq!(super::sidebar_child_indent(1), "      ");
        assert_eq!(super::sidebar_child_indent(2), "        ");
    }
}

// ── Right pane ────────────────────────────────────────────────────────────────
