#[cfg(test)]
mod tests {
    use crate::ca::CaStore;
    use crate::proxy::core::{NetworkDecision, ProxyState, SourceIdentityStatus};
    use crate::proxy::helpers::{
        bypass_host_matches, decode_source_from_proxy_authorization,
        ensure_host_header_matches_target, format_byte_count, proxy_authorization_matches_token,
        resolve_public_addrs, split_host_port,
    };
    use crate::proxy::http::{prompt_network, read_body_any};
    use crate::shared_config::SharedConfig;
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[test]
    fn decode_source_from_proxy_authorization_works() {
        let auth_payload = format!(
            "zcsrc:{}.{}",
            URL_SAFE_NO_PAD.encode("myproj"),
            URL_SAFE_NO_PAD.encode("mycont")
        );
        let header_value = format!("Basic {}", STANDARD.encode(auth_payload));
        let (project, container, status) = decode_source_from_proxy_authorization(&header_value);
        assert_eq!(status, SourceIdentityStatus::Ok);
        assert_eq!(project, Some("myproj".to_string()));
        assert_eq!(container, Some("mycont".to_string()));
    }

    #[test]
    fn proxy_authorization_matches_url_encoded_basic_credentials() {
        let header_value = format!("Basic {}", STANDARD.encode("harness%2Dhat:testtoken"));
        let headers = vec![("Proxy-Authorization".to_string(), header_value)];
        assert!(proxy_authorization_matches_token(&headers, "testtoken"));
        assert!(!proxy_authorization_matches_token(&headers, "wrong-token"));
    }

    #[test]
    fn format_byte_count_uses_compact_units() {
        assert_eq!(format_byte_count(0), "0b");
        assert_eq!(format_byte_count(512), "512b");
        assert_eq!(format_byte_count(1024), "1kb");
        assert_eq!(format_byte_count(1536), "1.5kb");
        assert_eq!(format_byte_count(10 * 1024), "10kb");
        assert_eq!(format_byte_count(1024 * 1024), "1mb");
    }

    #[tokio::test]
    async fn read_body_any_rejects_oversized_content_length() {
        let mut stream = tokio::io::empty();
        let headers = vec![("Content-Length".to_string(), "20000000".to_string())];

        let err = read_body_any(&mut stream, &headers, Vec::new())
            .await
            .expect_err("oversized body should fail");

        assert!(
            err.to_string().contains("request body too large"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bypass_host_matches_wildcards() {
        assert!(bypass_host_matches("*.google.com", "api.google.com"));
        assert!(bypass_host_matches("*.google.com", "google.com"));
        assert!(bypass_host_matches(".google.com", "api.google.com"));
        assert!(bypass_host_matches("google.com", "google.com"));
        assert!(!bypass_host_matches("google.com", "notgoogle.com"));
    }

    #[test]
    fn split_host_port_handles_authorities() {
        assert_eq!(
            split_host_port("example.com:8443", 443),
            ("example.com".to_string(), 8443)
        );
        assert_eq!(
            split_host_port("[2001:db8::1]:443", 80),
            ("2001:db8::1".to_string(), 443)
        );
        assert_eq!(
            split_host_port("2001:db8::1", 443),
            ("2001:db8::1".to_string(), 443)
        );
    }

    #[test]
    fn host_header_must_match_tls_target() {
        let headers = vec![("Host".to_string(), "Example.com:443".to_string())];
        ensure_host_header_matches_target(&headers, "example.com", 443)
            .expect("matching host header should be accepted");

        let headers = vec![("Host".to_string(), "example.com".to_string())];
        ensure_host_header_matches_target(&headers, "example.com", 443)
            .expect("missing default port should be accepted");

        let headers = vec![("Host".to_string(), "[2001:db8::1]:443".to_string())];
        ensure_host_header_matches_target(&headers, "2001:0db8::1", 443)
            .expect("equivalent IPv6 host header should be accepted");
    }

    #[test]
    fn host_header_rejects_mismatches_and_duplicates() {
        let headers = vec![("Host".to_string(), "blocked.example".to_string())];
        assert!(
            ensure_host_header_matches_target(&headers, "allowed.example", 443).is_err(),
            "mismatched host header should be rejected"
        );

        let headers = vec![("Host".to_string(), "example.com:444".to_string())];
        assert!(
            ensure_host_header_matches_target(&headers, "example.com", 443).is_err(),
            "mismatched host port should be rejected"
        );

        let headers = vec![
            ("Host".to_string(), "example.com".to_string()),
            ("host".to_string(), "example.com".to_string()),
        ];
        assert!(
            ensure_host_header_matches_target(&headers, "example.com", 443).is_err(),
            "duplicate host headers should be rejected"
        );
    }

    #[tokio::test]
    async fn resolve_public_addrs_rejects_restricted_ip_literals() {
        let err = resolve_public_addrs("127.0.0.1", 80)
            .await
            .expect_err("loopback should be rejected");
        assert!(
            err.to_string().contains("restricted address"),
            "unexpected error: {err}"
        );

        let err = resolve_public_addrs("::ffff:127.0.0.1", 80)
            .await
            .expect_err("IPv4-mapped loopback should be rejected");
        assert!(
            err.to_string().contains("restricted address"),
            "unexpected error: {err}"
        );

        resolve_public_addrs("8.8.8.8", 53)
            .await
            .expect("public IP literal should be accepted");
    }

    #[tokio::test]
    async fn prompt_network_sends_to_pending_tx() {
        let (_ca_tx, _ca_rx) = mpsc::channel::<()>(1); // dummy
        let (pending_tx, mut pending_rx) = mpsc::channel(1);
        let ca =
            Arc::new(CaStore::load_or_create(&std::env::temp_dir().join("proxy-test-ca")).unwrap());
        // Wait, I can just use build_test_app logic if I want but let's just make a dummy config.
        let raw = r#"
docker_dir = "/tmp"
[workspace]

[manager]
global_rules_file = "/tmp/global.toml""#;
        let cfg: crate::config::Config = toml::from_str(raw).unwrap();
        let (activity_tx, _activity_rx) = mpsc::unbounded_channel();
        let state = ProxyState::new(
            ca,
            SharedConfig::new(Arc::new(cfg)),
            pending_tx,
            activity_tx,
        )
        .unwrap();

        let prompt_task = tokio::spawn(async move {
            prompt_network(
                &state,
                "GET",
                "example.com",
                Some(443),
                "/test",
                Some("p".into()),
                Some("c".into()),
                "ok",
                true,
                None,
            )
            .await
        });

        // TUI side: receive the item
        let item = pending_rx
            .recv()
            .await
            .expect("should receive pending item");
        assert_eq!(item.host, "example.com");
        assert_eq!(item.port, Some(443));

        // TUI side: allow it
        item.response_tx.send(NetworkDecision::Allow).unwrap();

        let result = prompt_task.await.unwrap();
        assert!(result, "prompt_network should return true for Allow");
    }
}
