use super::{ComposedRules, NetworkPolicy, NetworkRules, ProjectRules, host_matches, load};
use super::{HostdoCommand, HostdoRules};

#[test]
fn load_parses_network_rules_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    std::fs::write(
        &path,
        r#"
version = 1

[network]
allowlist = ["domain=example.com", "method=CONNECT domain=*.example.com port=8443"]
denylist = ["domain=blocked.example.com"]
"#,
    )
    .expect("write rules");

    let rules = load(&path).expect("load rules");
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
    // Global auto command keeps `auto` — the global rules file is host-owned.
    assert_eq!(
        composed.match_hostdo(&["cargo".into(), "build".into()], None),
        Some(NetworkPolicy::Auto)
    );
    // Project (workspace) auto command is downgraded to `prompt`: the workspace
    // rules file is container-writable and must not grant passwordless host
    // execution (C1).
    assert_eq!(
        composed.match_hostdo(&["npm".into(), "test".into()], None),
        Some(NetworkPolicy::Prompt)
    );
    assert_eq!(composed.hostdo.default_policy, NetworkPolicy::Deny);
}

#[test]
fn compose_downgrades_workspace_auto_hostdo_to_prompt() {
    // A workspace rules file cannot grant `auto`; `deny` (tightening) is kept.
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
        Some(NetworkPolicy::Prompt),
        "workspace auto must be downgraded to prompt"
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

    // Pass as the host-owned global so `auto` is preserved and the test stays
    // focused on image distinction rather than the workspace downgrade (C1).
    let composed = ComposedRules::compose(&rules, &[]);
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
