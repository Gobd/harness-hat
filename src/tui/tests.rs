use super::{ContainerPickerState, SidebarItem};

#[test]
fn container_picker_steps_are_distinct() {
    assert_ne!(
        ContainerPickerState::NewSessionWorkspace { cursor: 0 },
        ContainerPickerState::NewSessionTemplate {
            workspace_idx: 0,
            cursor: 0
        }
    );
}

#[test]
fn sidebar_items_cover_session_and_creation_entries() {
    assert_eq!(SidebarItem::NewSession, SidebarItem::NewSession);
    assert_eq!(SidebarItem::Session(1), SidebarItem::Session(1));
    assert_ne!(SidebarItem::Session(1), SidebarItem::Session(2));
}
