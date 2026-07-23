use super::{
    ComposedRules, NetworkPolicy, NetworkRules, ProjectRules, append_hostdo_auto_approval,
    host_matches, load,
};
use super::{HostdoCommand, HostdoRules};

#[test]
fn load_parses_network_rules_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    std::fs::write(
        &path,
        r#"
version = 1
mirror_cwd = true

[network]
allowlist = ["domain=example.com", "method=CONNECT domain=*.example.com port=8443"]
denylist = ["domain=blocked.example.com"]
"#,
    )
    .expect("write rules");

    let rules = load(&path).expect("load rules");
    assert!(rules.mirror_cwd);
    assert_eq!(rules.network.allowlist.len(), 2);
    assert_eq!(rules.network.denylist.len(), 1);
}

#[test]
fn load_ignores_non_network_sections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    std::fs::write(
        &path,
        r#"
version = 1

[agentctl]
spawn_delay_ms = 500
max_subagents = 10

[exec]
default_policy = "prompt"
commands = []

[network]
allowlist = ["domain=api.openai.com"]
"#,
    )
    .expect("write rules");

    let rules = load(&path).expect("load rules with non-network sections");
    assert_eq!(rules.network.allowlist, ["domain=api.openai.com"]);
}

#[test]
fn compose_merges_network_rules_and_denies_win() {
    let global = ProjectRules {
        network: NetworkRules {
            allowlist: vec!["domain=example.com".to_string()],
            denylist: vec!["domain=blocked.example.com".to_string()],
        },
        ..ProjectRules::default()
    };
    let project = ProjectRules {
        network: NetworkRules {
            allowlist: vec!["domain=project.example.com".to_string()],
            denylist: Vec::new(),
        },
        ..ProjectRules::default()
    };

    let composed = ComposedRules::compose(&global, &[project]);
    assert_eq!(
        composed.match_network("GET", "example.com", "/"),
        NetworkPolicy::Auto
    );
    assert_eq!(
        composed.match_network("GET", "project.example.com", "/"),
        NetworkPolicy::Auto
    );
    assert_eq!(
        composed.match_network("GET", "blocked.example.com", "/"),
        NetworkPolicy::Deny
    );
}

#[test]
fn compose_enables_mirror_cwd_when_global_or_workspace_rules_opt_in() {
    let global = ProjectRules {
        mirror_cwd: true,
        ..ProjectRules::default()
    };
    assert!(ComposedRules::compose(&global, &[]).mirror_cwd);

    let workspace = ProjectRules {
        mirror_cwd: true,
        ..ProjectRules::default()
    };
    assert!(ComposedRules::compose(&ProjectRules::default(), &[workspace]).mirror_cwd);
    assert!(!ComposedRules::compose(&ProjectRules::default(), &[]).mirror_cwd);
}

#[test]
fn wildcard_domains_do_not_match_apex() {
    assert!(host_matches("*.example.com", "api.example.com"));
    assert!(!host_matches("*.example.com", "example.com"));
}

#[test]
fn load_parses_hostdo_rules() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    std::fs::write(
        &path,
        r#"
version = 1

[hostdo]
default_policy = "prompt"
commands = [{ argv = ["cargo", "build"], approval_mode = "auto", cwd = "$WORKSPACE" }]

[network]
allowlist = ["domain=example.com"]
"#,
    )
    .expect("write rules");

    let rules = load(&path).expect("load rules");
    assert_eq!(rules.hostdo.default_policy, NetworkPolicy::Prompt);
    assert_eq!(
        rules.hostdo.commands.len(),
        1,
        "one hostdo command should be parsed"
    );
    assert_eq!(rules.hostdo.commands[0].argv, vec!["cargo", "build"]);
    assert_eq!(rules.hostdo.commands[0].image, None);
    assert_eq!(rules.hostdo.commands[0].timeout_secs, 60);
}

#[test]
fn compose_hostdo_default_policy_and_exact_match() {
    let global = ProjectRules {
        hostdo: HostdoRules {
            default_policy: NetworkPolicy::Prompt,
            commands: vec![HostdoCommand {
                argv: vec!["cargo".into(), "build".into()],
                approval_mode: NetworkPolicy::Auto,
                ..HostdoCommand::default()
            }],
        },
        ..ProjectRules::default()
    };
    let project = ProjectRules {
        hostdo: HostdoRules {
            default_policy: NetworkPolicy::Deny,
            commands: vec![HostdoCommand {
                argv: vec!["npm".into(), "test".into()],
                approval_mode: NetworkPolicy::Auto,
                ..HostdoCommand::default()
            }],
        },
        ..ProjectRules::default()
    };

    let composed = ComposedRules::compose(&global, &[project]);
    assert_eq!(
        composed.match_hostdo(&["cargo".into(), "build".into()], None),
        Some(NetworkPolicy::Auto)
    );
    assert_eq!(
        composed.match_hostdo(&["npm".into(), "test".into()], None),
        Some(NetworkPolicy::Auto)
    );
    assert_eq!(composed.hostdo.default_policy, NetworkPolicy::Deny);
}

#[test]
fn compose_preserves_workspace_hostdo_decisions() {
    let project = ProjectRules {
        hostdo: HostdoRules {
            default_policy: NetworkPolicy::Prompt,
            commands: vec![
                HostdoCommand {
                    argv: vec!["rm".into(), "-rf".into(), "/".into()],
                    approval_mode: NetworkPolicy::Auto,
                    ..HostdoCommand::default()
                },
                HostdoCommand {
                    argv: vec!["curl".into(), "evil".into()],
                    approval_mode: NetworkPolicy::Deny,
                    ..HostdoCommand::default()
                },
            ],
        },
        ..ProjectRules::default()
    };

    let composed = ComposedRules::compose(&ProjectRules::default(), &[project]);
    assert_eq!(
        composed.match_hostdo(&["rm".into(), "-rf".into(), "/".into()], None),
        Some(NetworkPolicy::Auto),
        "workspace remembered allows must remain automatic"
    );
    assert_eq!(
        composed.match_hostdo(&["curl".into(), "evil".into()], None),
        Some(NetworkPolicy::Deny),
        "workspace deny must be preserved"
    );
}

#[test]
fn compose_hostdo_match_distinguishes_image() {
    let rules = ProjectRules {
        hostdo: HostdoRules {
            default_policy: NetworkPolicy::Prompt,
            commands: vec![HostdoCommand {
                argv: vec!["cargo".into(), "test".into()],
                image: Some("rust:1.88".into()),
                timeout_secs: 120,
                approval_mode: NetworkPolicy::Auto,
                ..HostdoCommand::default()
            }],
        },
        ..ProjectRules::default()
    };

    let composed = ComposedRules::compose(&ProjectRules::default(), &[rules]);
    assert_eq!(
        composed.match_hostdo(&["cargo".into(), "test".into()], Some("rust:1.88")),
        Some(NetworkPolicy::Auto)
    );
    assert_eq!(
        composed.match_hostdo(&["cargo".into(), "test".into()], None),
        None
    );
    assert_eq!(
        composed
            .find_hostdo(&["cargo".into(), "test".into()], Some("rust:1.88"))
            .map(|entry| entry.timeout_secs),
        Some(120)
    );
}

#[test]
fn load_parses_hostdo_env_allowlist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    std::fs::write(
        &path,
        r#"
version = 1

[hostdo]
commands = [{ argv = ["cargo", "test"], env_allowlist = ["CARGO_TERM_COLOR", "CI"] }]
"#,
    )
    .expect("write rules");

    let rules = load(&path).expect("load rules");
    assert_eq!(
        rules.hostdo.commands[0].env_allowlist.as_deref(),
        Some(["CARGO_TERM_COLOR".to_string(), "CI".to_string()].as_slice())
    );

    // Absent field stays None so the default inherit behavior is preserved.
    std::fs::write(
        &path,
        r#"
version = 1

[hostdo]
commands = [{ argv = ["cargo", "test"] }]
"#,
    )
    .expect("write rules");
    let rules = load(&path).expect("load rules");
    assert_eq!(rules.hostdo.commands[0].env_allowlist, None);
}

#[test]
fn load_rejects_invalid_env_allowlist_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    std::fs::write(
        &path,
        r#"
version = 1

[hostdo]
commands = [{ argv = ["make"], env_allowlist = ["BAD NAME"] }]
"#,
    )
    .expect("write rules");

    let err = load(&path).expect_err("invalid env name must be rejected");
    assert!(
        err.to_string().contains("env_allowlist"),
        "error should mention env_allowlist: {err}"
    );
}

#[test]
fn load_rejects_env_allowlist_on_image_rules() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    std::fs::write(
        &path,
        r#"
version = 1

[hostdo]
commands = [{ argv = ["npm", "test"], image = "node:20", env_allowlist = ["CI"] }]
"#,
    )
    .expect("write rules");

    let err = load(&path).expect_err("env_allowlist on an image rule must be rejected");
    assert!(
        err.to_string().contains("image rules"),
        "error should explain the image restriction: {err}"
    );
}

#[test]
fn remembered_workspace_hostdo_rule_is_persisted_and_effective() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    let argv = vec!["cargo".to_string(), "test".to_string()];

    assert!(
        append_hostdo_auto_approval(&path, &argv, None, 120).expect("persist remembered approval")
    );

    let workspace_rules = load(&path).expect("reload remembered approval");
    assert_eq!(workspace_rules.hostdo.commands.len(), 1);
    assert_eq!(
        workspace_rules.hostdo.commands[0].approval_mode,
        NetworkPolicy::Auto
    );

    let composed = ComposedRules::compose(&ProjectRules::default(), &[workspace_rules]);
    assert_eq!(
        composed.match_hostdo(&argv, None),
        Some(NetworkPolicy::Auto),
        "a remembered project-local decision must not prompt again"
    );
}
