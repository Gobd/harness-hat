use anyhow::{Context, Result, bail};
use std::time::Duration;

use crate::cli::ApprovalCommand;
use crate::server::{
    ApprovalAction, ApprovalActionRequest, ApprovalActionResponse, ErrorResponse,
    PendingApprovalRecord, PendingApprovalsResponse,
};

pub fn run(command: ApprovalCommand) -> Result<()> {
    let config_path = crate::manager::default_home_config_path()?;
    if !config_path.exists() {
        bail!(
            "global config does not exist at {}; run `hht install` first",
            config_path.display()
        );
    }
    let config = crate::config::load(&config_path)?;
    let token_path = config.logging.log_dir.join("token");
    let token = std::fs::read_to_string(&token_path).with_context(|| {
        format!(
            "reading daemon token at {}; is the daemon running?",
            token_path.display()
        )
    })?;
    let token = token.trim();
    if token.is_empty() {
        bail!("daemon token at {} is empty", token_path.display());
    }
    let control_url = format!(
        "http://{}:{}",
        config.defaults.control.server_host, config.defaults.control.server_port
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("building approvals client")?;

    match command {
        ApprovalCommand::List { json } => {
            let response = client
                .get(format!("{control_url}/approvals"))
                .bearer_auth(token)
                .send()
                .context("requesting pending approvals; is the daemon running?")?;
            let approvals: PendingApprovalsResponse = decode_response(response)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&approvals)?);
            } else {
                print_approvals_table(&approvals.approvals);
            }
        }
        ApprovalCommand::Allow { id, remember } => decide(
            &client,
            &control_url,
            token,
            &id,
            if remember {
                ApprovalAction::AllowForever
            } else {
                ApprovalAction::AllowOnce
            },
        )?,
        ApprovalCommand::Deny { id, remember } => decide(
            &client,
            &control_url,
            token,
            &id,
            if remember {
                ApprovalAction::DenyForever
            } else {
                ApprovalAction::DenyOnce
            },
        )?,
        ApprovalCommand::Trust { id } => {
            decide(&client, &control_url, token, &id, ApprovalAction::Trust)?
        }
    }
    Ok(())
}

fn decide(
    client: &reqwest::blocking::Client,
    control_url: &str,
    token: &str,
    id: &str,
    action: ApprovalAction,
) -> Result<()> {
    let response = client
        .post(format!("{control_url}/approvals/{id}"))
        .bearer_auth(token)
        .json(&ApprovalActionRequest { action })
        .send()
        .context("sending approval decision; is the daemon running?")?;
    let response: ApprovalActionResponse = decode_response(response)?;
    println!("{}: {}", response.id, response.message);
    Ok(())
}

fn decode_response<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> Result<T> {
    let status = response.status();
    let body = response.bytes().context("reading daemon response")?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_slice::<ErrorResponse>(&body) {
            bail!("{}: {}", error.error, error.reason);
        }
        bail!(
            "daemon request failed ({status}): {}",
            String::from_utf8_lossy(&body).trim()
        );
    }
    serde_json::from_slice(&body).context("decoding daemon response")
}

fn print_approvals_table(approvals: &[PendingApprovalRecord]) {
    if approvals.is_empty() {
        println!("No pending approvals.");
        return;
    }
    println!("ID    TYPE          WORKSPACE           DETAILS");
    for approval in approvals {
        let (id, kind, workspace, details) = match approval {
            PendingApprovalRecord::Network {
                id,
                workspace,
                method,
                host,
                port,
                path,
            } => {
                let port = port.map(|value| format!(":{value}")).unwrap_or_default();
                (
                    id,
                    "network",
                    workspace.as_deref().unwrap_or("-"),
                    format!("{method} {host}{port}{path}"),
                )
            }
            PendingApprovalRecord::Hostdo {
                id,
                workspace,
                argv,
                image,
                ..
            } => {
                let prefix = image
                    .as_deref()
                    .map(|value| format!("--image {value} "))
                    .unwrap_or_default();
                (
                    id,
                    "hostdo",
                    workspace.as_str(),
                    format!("{prefix}{}", argv.join(" ")),
                )
            }
            PendingApprovalRecord::RulesChange { id, path } => {
                (id, "rules-change", "-", path.clone())
            }
        };
        println!("{id:<5} {kind:<13} {workspace:<19} {details}");
    }
}

#[cfg(test)]
mod tests {
    use super::print_approvals_table;

    #[test]
    fn empty_approval_table_is_supported() {
        print_approvals_table(&[]);
    }
}
