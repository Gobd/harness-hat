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
