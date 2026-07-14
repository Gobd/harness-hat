use super::{ContainerPickerState, SidebarItem, move_wrapping_cursor, should_handle_key_event};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[test]
fn container_picker_steps_are_distinct() {
    assert_ne!(
        ContainerPickerState::NewSessionWorkspace { cursor: 0 },
        ContainerPickerState::NewSessionTemplate {
            workspace_idx: 0,
            cursor: 0,
            templates: Vec::new(),
        }
    );
}

#[test]
fn picker_cursor_wraps_at_boundaries() {
    let mut cursor = 0;
    move_wrapping_cursor(&mut cursor, 3, -1);
    assert_eq!(cursor, 2);

    move_wrapping_cursor(&mut cursor, 3, 1);
    assert_eq!(cursor, 0);

    cursor = 99;
    move_wrapping_cursor(&mut cursor, 3, 1);
    assert_eq!(cursor, 0);

    move_wrapping_cursor(&mut cursor, 0, 1);
    assert_eq!(cursor, 0);
}

#[test]
fn sidebar_items_cover_session_and_creation_entries() {
    assert_eq!(SidebarItem::NewSession, SidebarItem::NewSession);
    assert_eq!(SidebarItem::Session(1), SidebarItem::Session(1));
    assert_ne!(SidebarItem::Session(1), SidebarItem::Session(2));
}

#[test]
fn key_release_events_are_not_handled_as_repeated_input() {
    let key = |kind| KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, kind);

    assert!(should_handle_key_event(&key(KeyEventKind::Press)));
    assert!(should_handle_key_event(&key(KeyEventKind::Repeat)));
    assert!(!should_handle_key_event(&key(KeyEventKind::Release)));
}
