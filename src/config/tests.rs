use super::{
    Config, DefaultsConfig, image_tag_for_stem, load, resolve_workspace_sidebar_hotkeys,
};

fn temp_workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn defaults_include_control_server() {
    let defaults = DefaultsConfig::default();
    assert_eq!(defaults.control.server_port, 7878);
    assert_eq!(defaults.control.server_host, "127.0.0.1");
    assert_eq!(defaults.control.token_env_var, "HARNESS_HAT_TOKEN");
}

#[test]
fn image_tag_for_stem_is_stable() {
    assert_eq!(image_tag_for_stem("default"), "harness-hat-default:local");
    assert_eq!(image_tag_for_stem("rust.dev"), "harness-hat-rust.dev:local");
}

#[test]
fn load_resolves_template_resource_fields() {
    let root = temp_workspace();
    let workspace = root.path().join("repo");
    let docker_dir = root.path().join("docker");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&docker_dir).expect("docker");
    let config_path = root.path().join("harness-hat.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
version = 1
docker_dir = "{}"

[manager]
global_rules_file = "{}"

[[workspaces]]
name = "repo"
canonical_path = "{}"

[defaults.containers]
memory = "2g"
cpus = "1.5"
shm_size = "512m"
bypass_proxy = [
  "api.anthropic.com",
  "claude.ai",
  "platform.claude.com",
  "downloads.claude.ai",
  "storage.googleapis.com",
  "chatgpt.com",
  "*.chatgpt.com",
  "*.openai.com",
  "api.openai.com",
  "chat.openai.com",
  "auth.openai.com",
  "*.googleapis.com",
  "generativelanguage.googleapis.com",
  "aistudio.google.com",
  "accounts.google.com",
  "oauth2.googleapis.com",
  "www.googleapis.com",
  "openrouter.ai",
  "api.openrouter.ai",
]

[container_profiles.dev]
image = "default"
"#,
            docker_dir.display(),
            root.path().join("global-rules.toml").display(),
            workspace.display()
        ),
    )
    .expect("write config");

    let cfg = load(&config_path).expect("load config");
    let template = cfg.containers.iter().find(|ctr| ctr.name == "dev").unwrap();
    assert_eq!(template.image, "harness-hat-default:local");
    assert_eq!(template.memory.as_deref(), Some("2g"));
    assert_eq!(template.cpus.as_deref(), Some("1.5"));
    assert_eq!(template.shm_size.as_deref(), Some("512m"));
    let home = dirs::home_dir().expect("home dir");
    for (host, container) in [
        (home.join(".claude.json"), "/home/coder/.claude.json"),
        (home.join(".claude"), "/home/coder/.claude"),
        (home.join(".codex"), "/home/coder/.codex"),
        (home.join(".config/codex"), "/home/coder/.config/codex"),
        (home.join(".gemini"), "/home/coder/.gemini"),
        (home.join(".pi"), "/home/coder/.pi"),
    ] {
        assert!(
            template.mounts.iter().any(|mount| {
                mount.host == host && mount.container == std::path::PathBuf::from(container)
            }),
            "missing shared session mount {:?} -> {container}",
            host
        );
    }
    for host in [
        home.join(".claude.json"),
        home.join(".claude"),
        home.join(".codex"),
        home.join(".config/codex"),
        home.join(".gemini"),
        home.join(".pi"),
    ] {
        assert!(
            !template
                .mounts
                .iter()
                .any(|mount| mount.host == host && mount.container == host),
            "unexpected host-absolute shared session mount {:?}",
            host
        );
    }
    for host in [
        "api.anthropic.com",
        "claude.ai",
        "platform.claude.com",
        "downloads.claude.ai",
        "storage.googleapis.com",
        "chatgpt.com",
        "*.chatgpt.com",
        "*.openai.com",
        "api.openai.com",
        "chat.openai.com",
        "auth.openai.com",
        "*.googleapis.com",
        "generativelanguage.googleapis.com",
        "aistudio.google.com",
        "accounts.google.com",
        "oauth2.googleapis.com",
        "www.googleapis.com",
        "openrouter.ai",
        "api.openrouter.ai",
    ] {
        assert!(
            template.bypass_proxy.iter().any(|entry| entry == host),
            "missing shared bypass host {host}"
        );
    }
}

#[test]
fn workspace_hotkeys_are_assigned_without_duplicates() {
    let workspaces = vec![
        super::WorkspaceConfig {
            name: "alpha".to_string(),
            canonical_path: std::path::PathBuf::from("/tmp/a"),
            sidebar_hotkey: Some("z".to_string()),
        },
        super::WorkspaceConfig {
            name: "beta".to_string(),
            canonical_path: std::path::PathBuf::from("/tmp/b"),
            sidebar_hotkey: Some("z".to_string()),
        },
    ];
    let hotkeys = resolve_workspace_sidebar_hotkeys(&workspaces);
    assert_eq!(hotkeys[0], Some('z'));
    assert_ne!(hotkeys[0], hotkeys[1]);
}

#[test]
fn empty_config_default_is_valid_structurally() {
    let cfg = Config::default();
    assert!(cfg.containers.is_empty());
    assert!(cfg.workspaces.is_empty());
}
