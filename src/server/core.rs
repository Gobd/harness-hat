use axum::{
    Json,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::{mpsc, oneshot};

use crate::activity::{Activity, ActivityEvent, ActivityKind, ActivityState};
use crate::config::{MountMode, container_path_string, join_container_path};
use crate::container::docker_bind_mount_args;
use crate::rules::NetworkPolicy;
use crate::server::{
    ApprovalDecision, ErrorResponse, ExecJobKillResponse, ExecJobListResponse, ExecJobOutputQuery,
    ExecJobOutputResponse, ExecJobPhase, ExecJobProgress, ExecJobSendRequest, ExecJobSendResponse,
    ExecJobState, ExecJobStatus, ExecRequest, ExecResponse, PendingItem, ServerState, record_audit,
    require_session_context,
};
use crate::state::{AuditEntry, DecisionKind};

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const APPROVAL_TIMEOUT_SECS: u64 = 300;
const IMAGE_PULL_TIMEOUT_SECS: u64 = 30 * 60;
const EXEC_JOB_CAPTURE_BYTES: usize = 256 * 1024;
const EXEC_JOB_STDIN_QUEUE_CAPACITY: usize = 32;
const ACTIVITY_LINE_BUFFER_BYTES: usize = 64 * 1024;
/// Absolute ceiling on a hostdo command's wall-clock time. A container-supplied
/// `timeout_secs` cannot exceed this or the matched rule's own value (M4).
const MAX_TIMEOUT_SECS: u64 = 6 * 60 * 60;

/// 503 returned when the exec-job registry is at its active-job ceiling (H3).
fn job_capacity_exceeded_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            error: "too_many_jobs".into(),
            reason: "too many concurrent host commands are running; retry shortly".into(),
        }),
    )
        .into_response()
}

pub(super) async fn exec_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(req): Json<ExecRequest>,
) -> Response {
    // In-flight cap so a container cannot flood the exec path and spawn
    // unbounded concurrent host commands. Fast-fail with 503 rather than
    // queueing, mirroring the control endpoints (H3).
    let _exec_permit = match crate::server::handlers::hostdo_exec_semaphore().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return job_capacity_exceeded_response(),
    };
    let supports_jobs = supports_exec_jobs(&headers);
    let wants_detached = req.detach || supports_jobs;

    let (session_token, identity) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };

    if wants_detached && !state.exec_jobs.has_active_capacity() {
        return job_capacity_exceeded_response();
    }

    if req.argv.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "bad_request".into(),
                reason: "argv must contain at least one argument".into(),
            }),
        )
            .into_response();
    }
    if req.cwd.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "bad_request".into(),
                reason: "cwd must not be empty".into(),
            }),
        )
            .into_response();
    }

    let cfg = state.config.get();
    let Some(project) = cfg
        .workspaces
        .iter()
        .find(|workspace| workspace.name == identity.workspace_name)
    else {
        return deny_with_audit(
            &state,
            &identity.workspace_name,
            &req.argv,
            &req.cwd,
            "unknown workspace",
        )
        .await;
    };

    let host_cwd = match resolve_host_cwd_in_workspace(
        &req.cwd,
        Some(identity.mount_target.as_str()),
        &project.canonical_path,
    ) {
        Ok(path) => path,
        Err(reason) => {
            return deny_with_audit(
                &state,
                &identity.workspace_name,
                &req.argv,
                &req.cwd,
                reason,
            )
            .await;
        }
    };

    if let Err(e) = state
        .config
        .ensure_rules_trusted_for_workspace(Some(identity.workspace_name.as_str()))
    {
        return deny_with_audit(
            &state,
            &identity.workspace_name,
            &req.argv,
            &req.cwd,
            format!("rules file change requires review: {e}"),
        )
        .await;
    }

    let rules = match crate::config::load_composed_rules_for_workspace(
        &cfg,
        Some(identity.workspace_name.as_str()),
    ) {
        Ok(rules) => rules,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "invalid_rules".into(),
                    reason: e.to_string(),
                }),
            )
                .into_response();
        }
    };
    if let Err(e) = state
        .config
        .ensure_rules_trusted_for_workspace(Some(identity.workspace_name.as_str()))
    {
        return deny_with_audit(
            &state,
            &identity.workspace_name,
            &req.argv,
            &req.cwd,
            format!("rules file change requires review: {e}"),
        )
        .await;
    }

    let matched_rule = rules.find_hostdo(&req.argv, req.image.as_deref());
    let matched_command = matched_rule.and_then(|entry| entry.name.clone());
    let env_allowlist = matched_rule.and_then(|entry| entry.env_allowlist.clone());
    let approval_mode = matched_rule
        .map(|entry| entry.approval_mode)
        .unwrap_or(rules.hostdo.default_policy);
    if approval_mode == NetworkPolicy::Deny {
        return deny_with_audit(
            &state,
            &identity.workspace_name,
            &req.argv,
            &req.cwd,
            "command denied by hostdo policy",
        )
        .await;
    }

    // The matched rule's timeout is a ceiling, not just a fallback: a
    // container-supplied `timeout_secs` may lower it but never raise it, and an
    // absolute cap applies regardless (M4).
    let rule_ceiling = matched_rule
        .map(|entry| entry.timeout_secs)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let timeout_secs = req
        .timeout_secs
        .map(|requested| requested.min(rule_ceiling))
        .unwrap_or(rule_ceiling)
        .clamp(1, MAX_TIMEOUT_SECS);
    let runner_mount_target = container_path_string(Path::new(&identity.mount_target));
    let runner_cwd = resolve_runner_container_cwd(
        &req.cwd,
        &host_cwd,
        &runner_mount_target,
        &project.canonical_path,
    );
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let initial_state = if approval_mode == NetworkPolicy::Prompt {
        ActivityState::PendingApproval
    } else if req.image.is_some() {
        ActivityState::PullingImage
    } else {
        ActivityState::Running
    };
    let mut activity = Activity::new(
        identity.workspace_name.clone(),
        Some(identity.container_id.clone()),
        ActivityKind::Hostdo {
            argv: req.argv.clone(),
            image: req.image.clone(),
            timeout_secs,
            cwd: host_cwd.clone(),
        },
        initial_state,
        cancel_flag.clone(),
    );
    activity.session_token = Some(session_token.clone());
    let activity_id = activity.id.clone();
    let _ = state
        .activity_tx
        .try_send(ActivityEvent::Started(Box::new(activity)));

    let run = CommandRun {
        state: state.clone(),
        workspace_name: identity.workspace_name.clone(),
        session_token,
        container_id: identity.container_id.clone(),
        argv: req.argv.clone(),
        image: req.image.clone(),
        host_cwd: host_cwd.clone(),
        request_cwd: req.cwd.clone(),
        timeout_secs,
        workspace_path: project.canonical_path.clone(),
        mount_target: runner_mount_target,
        runner_cwd,
        activity_id,
        cancel_flag: cancel_flag.clone(),
        decision_kind: DecisionKind::Auto,
        env_allowlist,
    };

    if approval_mode == NetworkPolicy::Auto {
        if wants_detached {
            return start_execution_job(run);
        }
        return execute_immediate(run).await;
    }

    let (response_tx, response_rx) = oneshot::channel::<ApprovalDecision>();
    let pending = PendingItem {
        id: uuid::Uuid::new_v4().to_string(),
        activity_id: run.activity_id.clone(),
        cancel_flag,
        workspace_name: identity.workspace_name.clone(),
        container_id: Some(identity.container_id.clone()),
        argv: req.argv.clone(),
        image: req.image.clone(),
        timeout_secs,
        cwd: host_cwd.clone(),
        rule_cwd: PathBuf::from(&req.cwd),
        reason: req.reason.clone(),
        matched_command,
        response_tx: Some(response_tx),
    };
    if wants_detached {
        return start_approval_wait_job(run, response_rx, pending);
    }

    if state.pending_tx.send(pending).await.is_err() {
        let _ = state.activity_tx.try_send(ActivityEvent::Finished {
            id: run.activity_id.clone(),
            state: ActivityState::Failed,
            status: Some("manager is shutting down".to_string()),
        });
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "manager_shutting_down".into(),
                reason: "manager is shutting down".into(),
            }),
        )
            .into_response();
    }

    await_approval_and_execute(run, response_rx).await
}

pub(super) async fn exec_jobs_list_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    let (session_token, _) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    Json(ExecJobListResponse {
        jobs: state
            .exec_jobs
            .list_for_session(&session_token)
            .into_iter()
            .map(ExecJobStatus::without_output)
            .collect(),
    })
    .into_response()
}

pub(super) async fn exec_job_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    let (session_token, _) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let Some(job) = state.exec_jobs.get_for_session(&job_id, &session_token) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".into(),
                reason: "unknown exec job".into(),
            }),
        )
            .into_response();
    };
    Json(job.without_output()).into_response()
}

pub(super) async fn exec_job_output_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
    Query(query): Query<ExecJobOutputQuery>,
) -> Response {
    let (session_token, _) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let Some(job) = state.exec_jobs.get_for_session(&job_id, &session_token) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".into(),
                reason: "unknown exec job".into(),
            }),
        )
            .into_response();
    };

    let mut stdout = job.stdout.unwrap_or_default();
    let mut stderr = job.stderr.unwrap_or_default();
    if let Some(tail) = query.tail {
        stdout = tail_lines(&stdout, tail);
        stderr = tail_lines(&stderr, tail);
    }
    match query.stream.as_deref() {
        Some("stdout") => stderr.clear(),
        Some("stderr") => stdout.clear(),
        Some(_) | None => {}
    }

    Json(ExecJobOutputResponse {
        job_id,
        state: job.state,
        phase: job.phase,
        exit_code: job.exit_code,
        stdout,
        stderr,
        reason: job.reason,
    })
    .into_response()
}

pub(super) async fn exec_job_kill_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    let (session_token, _) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let Some(job) = state.exec_jobs.get_for_session(&job_id, &session_token) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".into(),
                reason: "unknown exec job".into(),
            }),
        )
            .into_response();
    };
    if let Some(cancel_flag) = job.cancel_flag {
        cancel_flag.store(true, Ordering::SeqCst);
        state.exec_jobs.update(&job_id, |status| {
            status.message = "Cancellation requested.".to_string();
            status.poll_after_ms = Some(250);
        });
        return Json(ExecJobKillResponse {
            ok: true,
            job_id,
            message: "Cancellation requested.".to_string(),
        })
        .into_response();
    }
    Json(ExecJobKillResponse {
        ok: true,
        job_id,
        message: "Job is no longer running.".to_string(),
    })
    .into_response()
}

pub(super) async fn exec_job_send_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
    Json(req): Json<ExecJobSendRequest>,
) -> Response {
    let (session_token, _) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let Some(job) = state.exec_jobs.get_for_session(&job_id, &session_token) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".into(),
                reason: "unknown exec job".into(),
            }),
        )
            .into_response();
    };
    let Some(stdin_tx) = job.stdin_tx else {
        return (
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "not_running".into(),
                reason: "job is not accepting input".into(),
            }),
        )
            .into_response();
    };
    match stdin_tx.try_send(req.input.into_bytes()) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse {
                    error: "input_backpressure".into(),
                    reason: "job input queue is full; retry shortly".into(),
                }),
            )
                .into_response();
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: "not_running".into(),
                    reason: "job input channel is closed".into(),
                }),
            )
                .into_response();
        }
    }
    Json(ExecJobSendResponse {
        ok: true,
        job_id,
        message: "Input forwarded.".to_string(),
    })
    .into_response()
}

struct CommandRun {
    state: Arc<ServerState>,
    workspace_name: String,
    session_token: String,
    container_id: String,
    argv: Vec<String>,
    image: Option<String>,
    host_cwd: PathBuf,
    request_cwd: String,
    timeout_secs: u64,
    workspace_path: PathBuf,
    mount_target: String,
    runner_cwd: String,
    activity_id: String,
    cancel_flag: Arc<AtomicBool>,
    decision_kind: DecisionKind,
    /// From the matched rule (M3): when set, the child runs from a cleared
    /// environment containing only `HOSTDO_BASE_ENV` plus these variables.
    env_allowlist: Option<Vec<String>>,
}

#[derive(Debug)]
struct ExecResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

#[derive(Debug)]
enum ExecFailure {
    TimedOut,
    Cancelled,
    Message(String),
}

async fn await_approval_and_execute(
    run: CommandRun,
    response_rx: oneshot::Receiver<ApprovalDecision>,
) -> Response {
    let approval_start = Instant::now();
    let decision =
        match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), response_rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) | Err(_) => {
                let _ = run.state.activity_tx.try_send(ActivityEvent::Finished {
                    id: run.activity_id.clone(),
                    state: ActivityState::Failed,
                    status: Some("approval timed out".to_string()),
                });
                record_command_audit(
                    &run.state,
                    &run.workspace_name,
                    &run.argv,
                    &run.request_cwd,
                    DecisionKind::TimedOut,
                    None,
                    None,
                )
                .await;
                return (
                    StatusCode::REQUEST_TIMEOUT,
                    Json(ErrorResponse {
                        error: "timed_out".into(),
                        reason: "approval timed out (5 minutes)".into(),
                    }),
                )
                    .into_response();
            }
        };
    let _ = approval_start;

    match decision {
        ApprovalDecision::Deny => {
            let state_label = if run.cancel_flag.load(Ordering::SeqCst) {
                ActivityState::Cancelled
            } else {
                ActivityState::Denied
            };
            let status = if state_label == ActivityState::Cancelled {
                "cancelled"
            } else {
                "denied by developer"
            };
            let _ = run.state.activity_tx.try_send(ActivityEvent::Finished {
                id: run.activity_id.clone(),
                state: state_label,
                status: Some(status.to_string()),
            });
            record_command_audit(
                &run.state,
                &run.workspace_name,
                &run.argv,
                &run.request_cwd,
                DecisionKind::Denied,
                None,
                None,
            )
            .await;
            (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse {
                    error: "denied".into(),
                    reason: "denied by developer".into(),
                }),
            )
                .into_response()
        }
        ApprovalDecision::Approve { remember } => {
            let mut run = run;
            run.decision_kind = if remember {
                DecisionKind::Remembered
            } else {
                DecisionKind::Approved
            };
            execute_immediate(run).await
        }
    }
}

fn start_approval_wait_job(
    run: CommandRun,
    response_rx: oneshot::Receiver<ApprovalDecision>,
    pending: PendingItem,
) -> Response {
    let (stdin_tx, stdin_rx) = mpsc::channel(EXEC_JOB_STDIN_QUEUE_CAPACITY);
    let Some(status) = run.state.exec_jobs.insert(ExecJobStatus {
        state: ExecJobState::Running,
        job_id: String::new(),
        workspace_name: run.workspace_name.clone(),
        session_token: run.session_token.clone(),
        container: Some(run.container_id.clone()),
        timeout_secs: run.timeout_secs,
        argv: run.argv.clone(),
        cwd: Some(run.host_cwd.display().to_string()),
        phase: Some(ExecJobPhase::PendingApproval),
        image: run.image.clone(),
        message: "Waiting for developer approval.".to_string(),
        progress: None,
        poll_after_ms: Some(1000),
        exit_code: None,
        stdout: Some(String::new()),
        stderr: Some(String::new()),
        reason: None,
        cancel_flag: Some(run.cancel_flag.clone()),
        stdin_tx: Some(stdin_tx),
        created_at: Instant::now(),
    }) else {
        run.cancel_flag.store(true, Ordering::SeqCst);
        let _ = run.state.activity_tx.try_send(ActivityEvent::Finished {
            id: run.activity_id.clone(),
            state: ActivityState::Failed,
            status: Some("exec job capacity reached".to_string()),
        });
        return job_capacity_exceeded_response();
    };
    let job_id = status.job_id.clone();
    if run.state.pending_tx.try_send(pending).is_err() {
        run.state.exec_jobs.update(&job_id, |status| {
            status.state = ExecJobState::Failed;
            status.phase = None;
            status.message = "approval queue is full or closed".to_string();
            status.reason = Some("approval queue is full or closed".to_string());
            status.poll_after_ms = None;
            status.cancel_flag = None;
            status.stdin_tx = None;
        });
        let _ = run.state.activity_tx.try_send(ActivityEvent::Finished {
            id: run.activity_id.clone(),
            state: ActivityState::Failed,
            status: Some("approval queue is full or closed".to_string()),
        });
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "busy".into(),
                reason: "approval queue is full or closed".into(),
            }),
        )
            .into_response();
    }
    let run_clone = run;
    tokio::spawn(async move {
        let decision =
            match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), response_rx)
                .await
            {
                Ok(Ok(decision)) => decision,
                Ok(Err(_)) | Err(_) => {
                    run_clone.state.exec_jobs.update(&job_id, |status| {
                        status.state = ExecJobState::Failed;
                        status.phase = None;
                        status.message = "approval timed out (5 minutes)".to_string();
                        status.poll_after_ms = None;
                        status.reason = Some("approval timed out (5 minutes)".to_string());
                        status.cancel_flag = None;
                        status.stdin_tx = None;
                    });
                    let _ = run_clone
                        .state
                        .activity_tx
                        .try_send(ActivityEvent::Finished {
                            id: run_clone.activity_id.clone(),
                            state: ActivityState::Failed,
                            status: Some("approval timed out".to_string()),
                        });
                    record_command_audit(
                        &run_clone.state,
                        &run_clone.workspace_name,
                        &run_clone.argv,
                        &run_clone.request_cwd,
                        DecisionKind::TimedOut,
                        None,
                        None,
                    )
                    .await;
                    return;
                }
            };

        match decision {
            ApprovalDecision::Deny => {
                let state_label = if run_clone.cancel_flag.load(Ordering::SeqCst) {
                    ActivityState::Cancelled
                } else {
                    ActivityState::Denied
                };
                let reason = if state_label == ActivityState::Cancelled {
                    "cancelled"
                } else {
                    "denied by developer"
                };
                run_clone.state.exec_jobs.update(&job_id, |status| {
                    status.state = ExecJobState::Failed;
                    status.phase = None;
                    status.message = reason.to_string();
                    status.poll_after_ms = None;
                    status.reason = Some(reason.to_string());
                    status.cancel_flag = None;
                    status.stdin_tx = None;
                });
                let _ = run_clone
                    .state
                    .activity_tx
                    .try_send(ActivityEvent::Finished {
                        id: run_clone.activity_id.clone(),
                        state: state_label,
                        status: Some(reason.to_string()),
                    });
                record_command_audit(
                    &run_clone.state,
                    &run_clone.workspace_name,
                    &run_clone.argv,
                    &run_clone.request_cwd,
                    DecisionKind::Denied,
                    None,
                    None,
                )
                .await;
            }
            ApprovalDecision::Approve { remember } => {
                let mut run = run_clone;
                run.decision_kind = if remember {
                    DecisionKind::Remembered
                } else {
                    DecisionKind::Approved
                };
                run_job_after_approval(run, job_id, stdin_rx).await;
            }
        }
    });

    (StatusCode::ACCEPTED, Json(status)).into_response()
}

fn start_execution_job(run: CommandRun) -> Response {
    let phase = if run.image.is_some() {
        ExecJobPhase::PullingImage
    } else {
        ExecJobPhase::StartingCommand
    };
    let message = if let Some(image) = &run.image {
        format!("Preparing Docker image '{image}'.")
    } else {
        format!("Starting {}.", run.argv.join(" "))
    };
    let (stdin_tx, stdin_rx) = mpsc::channel(EXEC_JOB_STDIN_QUEUE_CAPACITY);
    let Some(status) = run.state.exec_jobs.insert(ExecJobStatus {
        state: ExecJobState::Running,
        job_id: String::new(),
        workspace_name: run.workspace_name.clone(),
        session_token: run.session_token.clone(),
        container: Some(run.container_id.clone()),
        timeout_secs: run.timeout_secs,
        argv: run.argv.clone(),
        cwd: Some(run.host_cwd.display().to_string()),
        phase: Some(phase),
        image: run.image.clone(),
        message,
        progress: None,
        poll_after_ms: Some(1000),
        exit_code: None,
        stdout: Some(String::new()),
        stderr: Some(String::new()),
        reason: None,
        cancel_flag: Some(run.cancel_flag.clone()),
        stdin_tx: Some(stdin_tx),
        created_at: Instant::now(),
    }) else {
        let _ = run.state.activity_tx.try_send(ActivityEvent::Finished {
            id: run.activity_id.clone(),
            state: ActivityState::Failed,
            status: Some("exec job capacity reached".to_string()),
        });
        return job_capacity_exceeded_response();
    };
    let job_id = status.job_id.clone();
    tokio::spawn(async move {
        run_job_after_approval(run, job_id, stdin_rx).await;
    });
    (StatusCode::ACCEPTED, Json(status)).into_response()
}

async fn execute_immediate(run: CommandRun) -> Response {
    match run_command(run, None).await {
        Ok(result) => Json(ExecResponse {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
        })
        .into_response(),
        Err(ExecFailure::TimedOut) => (
            StatusCode::REQUEST_TIMEOUT,
            Json(ErrorResponse {
                error: "timed_out".into(),
                reason: "host command timed out".into(),
            }),
        )
            .into_response(),
        Err(ExecFailure::Cancelled) => (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "cancelled".into(),
                reason: "command cancelled".into(),
            }),
        )
            .into_response(),
        Err(ExecFailure::Message(reason)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "execution_failed".into(),
                reason,
            }),
        )
            .into_response(),
    }
}

async fn run_job_after_approval(
    run: CommandRun,
    job_id: String,
    stdin_rx: mpsc::Receiver<Vec<u8>>,
) {
    let state = run.state.clone();
    if let Err(error) = run_command(run, Some((job_id.clone(), stdin_rx))).await {
        let reason = match error {
            ExecFailure::TimedOut => "host command timed out".to_string(),
            ExecFailure::Cancelled => "command cancelled".to_string(),
            ExecFailure::Message(reason) => reason,
        };
        state.exec_jobs.update(&job_id, |status| {
            if status.state == ExecJobState::Running {
                status.state = ExecJobState::Failed;
                status.phase = None;
                status.message = reason.clone();
                status.reason = Some(reason);
                status.poll_after_ms = None;
                status.cancel_flag = None;
                status.stdin_tx = None;
            }
        });
    }
}

async fn run_command(
    run: CommandRun,
    job: Option<(String, mpsc::Receiver<Vec<u8>>)>,
) -> Result<ExecResult, ExecFailure> {
    let job_id = job.as_ref().map(|(job_id, _)| job_id.clone());
    if let Some(image) = &run.image {
        if let Err(err) = ensure_image_present(&run, image, job_id.as_deref()).await {
            let (activity_state, msg) = exec_failure_state(&err, run.timeout_secs);
            let _ = run.state.activity_tx.try_send(ActivityEvent::Finished {
                id: run.activity_id.clone(),
                state: activity_state,
                status: Some(msg.clone()),
            });
            if let Some(job_id) = &job_id {
                run.state.exec_jobs.update(job_id, |status| {
                    status.state = ExecJobState::Failed;
                    status.phase = None;
                    status.message = msg;
                    status.progress = None;
                    status.poll_after_ms = None;
                    status.cancel_flag = None;
                    status.stdin_tx = None;
                });
            }
            return Err(err);
        }
    }

    let mut cmd = build_command(&run)?;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| ExecFailure::Message(format!("failed to spawn command: {e}")))?;
    let started_at = Instant::now();
    let _ = run.state.activity_tx.try_send(ActivityEvent::State {
        id: run.activity_id.clone(),
        state: ActivityState::Running,
        status: Some("running command".to_string()),
    });
    if let Some(job_id) = &job_id {
        run.state.exec_jobs.update(job_id, |status| {
            status.state = ExecJobState::Running;
            status.phase = Some(ExecJobPhase::RunningCommand);
            status.message = format!("Running {}.", run.argv.join(" "));
            status.progress = None;
            status.poll_after_ms = Some(1000);
        });
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExecFailure::Message("command produced no stdout pipe".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExecFailure::Message("command produced no stderr pipe".into()))?;

    let stdout_task = spawn_stream_reader(
        stdout,
        run.state.clone(),
        run.activity_id.clone(),
        job_id.clone(),
        false,
    );
    let stderr_task = spawn_stream_reader(
        stderr,
        run.state.clone(),
        run.activity_id.clone(),
        job_id.clone(),
        true,
    );

    if let Some((_, mut stdin_rx)) = job {
        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                while let Some(bytes) = stdin_rx.recv().await {
                    if stdin.write_all(&bytes).await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
            });
        }
    }

    let status = match wait_for_child(&mut child, run.cancel_flag.clone(), run.timeout_secs).await {
        Ok(status) => status,
        Err(err) => {
            stdout_task.abort();
            stderr_task.abort();
            let duration_ms = started_at.elapsed().as_millis() as u64;
            let (activity_state, msg) = exec_failure_state(&err, run.timeout_secs);
            record_command_audit(
                &run.state,
                &run.workspace_name,
                &run.argv,
                &run.request_cwd,
                run.decision_kind.clone(),
                None,
                Some(duration_ms),
            )
            .await;
            let _ = run.state.activity_tx.try_send(ActivityEvent::Finished {
                id: run.activity_id.clone(),
                state: activity_state,
                status: Some(msg.clone()),
            });
            if let Some(job_id) = &job_id {
                run.state.exec_jobs.update(job_id, |status| {
                    status.state = ExecJobState::Failed;
                    status.phase = None;
                    status.message = msg;
                    status.progress = None;
                    status.poll_after_ms = None;
                    status.cancel_flag = None;
                    status.stdin_tx = None;
                });
            }
            return Err(err);
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|e| ExecFailure::Message(format!("stdout task failed: {e}")))??;
    let stderr = stderr_task
        .await
        .map_err(|e| ExecFailure::Message(format!("stderr task failed: {e}")))??;
    let exit_code = status.code().unwrap_or(-1);
    let duration_ms = started_at.elapsed().as_millis() as u64;

    record_command_audit(
        &run.state,
        &run.workspace_name,
        &run.argv,
        &run.request_cwd,
        run.decision_kind.clone(),
        Some(exit_code),
        Some(duration_ms),
    )
    .await;
    let _ = run.state.activity_tx.try_send(ActivityEvent::Finished {
        id: run.activity_id.clone(),
        state: ActivityState::Complete,
        status: Some(format!("exit code {exit_code}")),
    });
    if let Some(job_id) = &job_id {
        run.state.exec_jobs.update(job_id, |status| {
            status.state = ExecJobState::Complete;
            status.phase = None;
            status.message = format!("Command finished with exit code {exit_code}.");
            status.progress = None;
            status.poll_after_ms = None;
            status.exit_code = Some(exit_code);
            status.stdout = Some(stdout.clone());
            status.stderr = Some(stderr.clone());
            status.reason = None;
            status.cancel_flag = None;
            status.stdin_tx = None;
        });
    }

    Ok(ExecResult {
        exit_code,
        stdout,
        stderr,
    })
}

async fn ensure_image_present(
    run: &CommandRun,
    image: &str,
    job_id: Option<&str>,
) -> Result<(), ExecFailure> {
    if docker_image_present(image).await? {
        return Ok(());
    }

    let _ = run.state.activity_tx.try_send(ActivityEvent::State {
        id: run.activity_id.clone(),
        state: ActivityState::PullingImage,
        status: Some(format!("pulling Docker image '{image}'")),
    });
    if let Some(job_id) = job_id {
        run.state.exec_jobs.update(job_id, |status| {
            status.state = ExecJobState::Running;
            status.phase = Some(ExecJobPhase::PullingImage);
            status.image = Some(image.to_string());
            status.message = format!("Pulling Docker image '{image}'.");
            status.progress = Some(ExecJobProgress {
                kind: "indeterminate".to_string(),
                id: None,
                status: None,
                detail: None,
            });
            status.poll_after_ms = Some(1000);
        });
    }

    let mut cmd = TokioCommand::new("docker");
    cmd.arg("pull")
        .arg(image)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| ExecFailure::Message(format!("failed to spawn docker pull: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExecFailure::Message("docker pull produced no stdout pipe".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExecFailure::Message("docker pull produced no stderr pipe".into()))?;
    let stdout_task = spawn_stream_reader(
        stdout,
        run.state.clone(),
        run.activity_id.clone(),
        job_id.map(str::to_string),
        false,
    );
    let stderr_task = spawn_stream_reader(
        stderr,
        run.state.clone(),
        run.activity_id.clone(),
        job_id.map(str::to_string),
        true,
    );

    let status =
        match wait_for_child(&mut child, run.cancel_flag.clone(), IMAGE_PULL_TIMEOUT_SECS).await {
            Ok(s) => s,
            Err(err) => {
                stdout_task.abort();
                stderr_task.abort();
                return Err(err);
            }
        };
    let _ = stdout_task
        .await
        .map_err(|e| ExecFailure::Message(format!("docker pull stdout task failed: {e}")))??;
    let _ = stderr_task
        .await
        .map_err(|e| ExecFailure::Message(format!("docker pull stderr task failed: {e}")))??;
    if !status.success() {
        return Err(ExecFailure::Message(format!(
            "docker pull failed with exit code {}",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

fn build_command(run: &CommandRun) -> Result<TokioCommand, ExecFailure> {
    if run.argv.is_empty() {
        return Err(ExecFailure::Message("command argv is empty".into()));
    }
    let mut cmd = if let Some(image) = &run.image {
        validate_hostdo_image(image)?;
        let mut cmd = TokioCommand::new("docker");
        cmd.arg("run").arg("--rm").arg("-i");
        for arg in docker_bind_mount_args(
            &run.workspace_path.display().to_string(),
            &run.mount_target,
            &MountMode::Rw,
        )
        .map_err(|e| ExecFailure::Message(e.to_string()))?
        {
            cmd.arg(arg);
        }
        cmd.arg("-w")
            .arg(&run.runner_cwd)
            // `--` terminates option parsing so a crafted image/argv value
            // beginning with `-` cannot be reinterpreted as a `docker run` flag
            // (M2). validate_hostdo_image already rejects a leading `-`; this is
            // belt-and-suspenders.
            .arg("--")
            .arg(image);
        for arg in &run.argv {
            cmd.arg(arg);
        }
        cmd
    } else {
        let mut cmd = TokioCommand::new(&run.argv[0]);
        cmd.args(&run.argv[1..]).current_dir(&run.host_cwd);
        cmd
    };
    // Strip harness-hat's own control-plane secrets from the child environment.
    // A hostdo command is a host build/test tool and has no business reading the
    // control token, session token, or scoped-proxy credential it would
    // otherwise inherit from the manager process (M3).
    for (name, _) in std::env::vars() {
        if name.starts_with("HARNESS_HAT_") {
            cmd.env_remove(name);
        }
    }
    // When the matched rule opts in via env_allowlist, go further: start from a
    // cleared environment and pass through only the base set plus the
    // allowlisted variables (M3). Rust resolves argv[0] against the PATH set on
    // the command itself, so lookup still works after env_clear.
    if let Some(vars) = hostdo_child_env(run.env_allowlist.as_deref(), &|name| {
        std::env::var(name).ok()
    }) {
        cmd.env_clear();
        cmd.envs(vars);
    }
    // Place each spawned process in its own process group so kill_child_and_group
    // can reach all descendants, not just the direct child.
    #[cfg(unix)]
    cmd.process_group(0);
    Ok(cmd)
}

/// Variables every hostdo child keeps when a rule's `env_allowlist` triggers an
/// environment scrub: enough for shells and build tools to locate binaries,
/// caches, and temp space without leaking the rest of the manager's env (M3).
const HOSTDO_BASE_ENV: [&str; 10] = [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM",
];

/// Compute the full environment for a hostdo child when the matched rule set
/// an `env_allowlist` (M3). Returns `None` when no allowlist applies — the
/// child then inherits the manager environment (minus `HARNESS_HAT_*`, stripped
/// separately). With an allowlist, the child gets exactly the base set plus the
/// allowlisted names, each only if present in the host environment.
/// `HARNESS_HAT_*` control-plane variables are never passed, even if listed.
fn hostdo_child_env(
    allowlist: Option<&[String]>,
    host_env: &dyn Fn(&str) -> Option<String>,
) -> Option<Vec<(String, String)>> {
    let allowlist = allowlist?;
    let mut vars: Vec<(String, String)> = Vec::new();
    let push = |name: &str, vars: &mut Vec<(String, String)>| {
        if name.starts_with("HARNESS_HAT_") || vars.iter().any(|(existing, _)| existing == name) {
            return;
        }
        if let Some(value) = host_env(name) {
            vars.push((name.to_string(), value));
        }
    };
    for name in HOSTDO_BASE_ENV {
        push(name, &mut vars);
    }
    for name in allowlist {
        push(name, &mut vars);
    }
    Some(vars)
}

/// Reject an obviously unsafe container image reference before it reaches the
/// `docker run` argument vector (M2). A value beginning with `-` would be parsed
/// as a flag; empty/whitespace or control characters indicate a malformed or
/// hostile request. The character set matches a Docker image reference
/// (`registry/name:tag@digest`).
fn validate_hostdo_image(image: &str) -> Result<(), ExecFailure> {
    if image.is_empty() {
        return Err(ExecFailure::Message("image reference is empty".into()));
    }
    if image.starts_with('-') {
        return Err(ExecFailure::Message(format!(
            "image reference must not begin with '-': {image:?}"
        )));
    }
    if !image
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '@'))
    {
        return Err(ExecFailure::Message(format!(
            "image reference contains invalid characters: {image:?}"
        )));
    }
    Ok(())
}

async fn docker_image_present(image: &str) -> Result<bool, ExecFailure> {
    let status = TokioCommand::new("docker")
        .arg("image")
        .arg("inspect")
        .arg(image)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| ExecFailure::Message(format!("failed checking docker image: {e}")))?;
    Ok(status.success())
}

async fn kill_child_and_group(child: &mut tokio::process::Child) {
    // Kill the entire process group so child subprocesses don't outlive the timeout.
    // The process group ID equals the child PID when process_group(0) is set in build_command.
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let pid = pid.to_string();
        let _ = TokioCommand::new("taskkill")
            .args(["/PID", pid.as_str(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn wait_for_child(
    child: &mut tokio::process::Child,
    cancel_flag: Arc<AtomicBool>,
    timeout_secs: u64,
) -> Result<std::process::ExitStatus, ExecFailure> {
    let started = Instant::now();
    loop {
        if cancel_flag.load(Ordering::SeqCst) {
            kill_child_and_group(child).await;
            return Err(ExecFailure::Cancelled);
        }
        if started.elapsed() >= Duration::from_secs(timeout_secs) {
            kill_child_and_group(child).await;
            return Err(ExecFailure::TimedOut);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|e| ExecFailure::Message(format!("failed waiting for command: {e}")))?
        {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn exec_failure_state(err: &ExecFailure, timeout_secs: u64) -> (ActivityState, String) {
    match err {
        ExecFailure::TimedOut => (
            ActivityState::Failed,
            format!("timed out after {timeout_secs}s"),
        ),
        ExecFailure::Cancelled => (ActivityState::Cancelled, "cancelled".to_string()),
        ExecFailure::Message(m) => (ActivityState::Failed, m.clone()),
    }
}

fn spawn_stream_reader<R>(
    mut reader: R,
    state: Arc<ServerState>,
    activity_id: String,
    job_id: Option<String>,
    is_stderr: bool,
) -> tokio::task::JoinHandle<Result<String, ExecFailure>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = String::new();
        let mut activity_line = String::new();
        let mut chunk = [0u8; 8192];
        loop {
            let count = reader
                .read(&mut chunk)
                .await
                .map_err(|e| ExecFailure::Message(format!("failed reading process output: {e}")))?;
            if count == 0 {
                break;
            }
            let bytes = &chunk[..count];
            append_captured_bytes(&mut buf, bytes);
            if let Some(job_id) = &job_id {
                state.exec_jobs.update(job_id, |status| {
                    let output = if is_stderr {
                        &mut status.stderr
                    } else {
                        &mut status.stdout
                    };
                    append_job_output(output, bytes);
                });
            }
            activity_line.push_str(&String::from_utf8_lossy(bytes));
            emit_complete_activity_lines(&state, &activity_id, is_stderr, &mut activity_line);
        }
        if !activity_line.is_empty() {
            emit_activity_line(
                &state,
                &activity_id,
                is_stderr,
                std::mem::take(&mut activity_line),
            );
        }
        Ok(buf)
    })
}

fn emit_complete_activity_lines(
    state: &ServerState,
    activity_id: &str,
    is_stderr: bool,
    pending: &mut String,
) {
    while let Some(newline) = pending.find('\n') {
        let mut line = pending.drain(..=newline).collect::<String>();
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
        emit_activity_line(state, activity_id, is_stderr, line);
    }
    while pending.len() > ACTIVITY_LINE_BUFFER_BYTES {
        let split = floor_char_boundary(pending, ACTIVITY_LINE_BUFFER_BYTES);
        let line = pending.drain(..split).collect::<String>();
        emit_activity_line(state, activity_id, is_stderr, line);
    }
}

fn emit_activity_line(state: &ServerState, activity_id: &str, is_stderr: bool, line: String) {
    let prefix = if is_stderr { "stderr" } else { "stdout" };
    let _ = state.activity_tx.try_send(ActivityEvent::Line {
        id: activity_id.to_string(),
        line: format!("{prefix}: {line}"),
    });
}

async fn deny_with_audit(
    state: &ServerState,
    project: &str,
    argv: &[String],
    cwd: &str,
    reason: impl Into<String>,
) -> Response {
    record_command_audit(
        state,
        project,
        argv,
        cwd,
        DecisionKind::DeniedByPolicy,
        None,
        None,
    )
    .await;
    (
        StatusCode::FORBIDDEN,
        Json(ErrorResponse {
            error: "denied_by_policy".into(),
            reason: reason.into(),
        }),
    )
        .into_response()
}

async fn record_command_audit(
    state: &ServerState,
    project: &str,
    argv: &[String],
    cwd: &str,
    decision: DecisionKind,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
) {
    record_audit(
        state,
        AuditEntry {
            workspace_name: project.to_string(),
            argv: argv.to_vec(),
            cwd: cwd.to_string(),
            decision,
            exit_code,
            duration_ms,
            timestamp: Utc::now(),
        },
    )
    .await;
}

fn supports_exec_jobs(headers: &HeaderMap) -> bool {
    headers
        .get("x-hostdo-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("jobs"))
        })
}

fn resolve_host_cwd_in_workspace(
    request_cwd: &str,
    mount_target: Option<&str>,
    workspace_host_path: &Path,
) -> Result<PathBuf, String> {
    let workspace_root = workspace_host_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_host_path.to_path_buf());
    if let Some(mount_target) = mount_target {
        let mount_target = mount_target.replace('\\', "/");
        let request_cwd_posix = request_cwd.replace('\\', "/");
        if request_cwd_posix == mount_target {
            return Ok(workspace_root);
        }
        if let Some(rel) = strip_container_path_prefix(&request_cwd_posix, &mount_target) {
            let host_rel = posix_relative_to_host_path(rel);
            return confine_host_cwd_to_workspace(&workspace_root.join(host_rel), &workspace_root);
        }
    }
    confine_host_cwd_to_workspace(Path::new(request_cwd), &workspace_root)
}

pub(super) fn confine_host_cwd_to_workspace(
    path: &Path,
    workspace_host_path: &Path,
) -> Result<PathBuf, String> {
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("hostdo cwd is not accessible: {}: {e}", path.display()))?;
    let workspace_root = workspace_host_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_host_path.to_path_buf());
    if resolved == workspace_root || resolved.starts_with(&workspace_root) {
        Ok(resolved)
    } else {
        Err(format!(
            "hostdo cwd is outside workspace '{}': {}",
            workspace_root.display(),
            resolved.display()
        ))
    }
}

fn resolve_runner_container_cwd(
    request_cwd: &str,
    host_cwd: &Path,
    mount_target: &str,
    workspace_host_path: &Path,
) -> String {
    let workspace_root = workspace_host_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_host_path.to_path_buf());
    if host_cwd == workspace_root.as_path() || host_cwd.starts_with(&workspace_root) {
        if let Ok(rel) = host_cwd.strip_prefix(&workspace_root) {
            return join_container_path(mount_target, rel);
        }
    }
    let request_cwd = request_cwd.replace('\\', "/");
    if request_cwd.starts_with('/') {
        request_cwd
    } else {
        mount_target.to_string()
    }
}

fn strip_container_path_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return path.strip_prefix('/');
    }
    if path == prefix {
        return Some("");
    }
    path.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
}

fn posix_relative_to_host_path(rel: &str) -> PathBuf {
    rel.split('/')
        .filter(|part| !part.is_empty())
        .collect::<PathBuf>()
}

fn append_captured_bytes(output: &mut String, bytes: &[u8]) {
    if output.len() >= EXEC_JOB_CAPTURE_BYTES {
        return;
    }
    let remaining = EXEC_JOB_CAPTURE_BYTES - output.len();
    let decoded = String::from_utf8_lossy(bytes);
    let take = floor_char_boundary(&decoded, remaining.min(decoded.len()));
    output.push_str(&decoded[..take]);
}

fn append_job_output(output: &mut Option<String>, bytes: &[u8]) {
    let buf = output.get_or_insert_with(String::new);
    append_captured_bytes(buf, bytes);
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn tail_lines(text: &str, count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(count);
    let mut out = lines[start..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        EXEC_JOB_CAPTURE_BYTES, append_captured_bytes, floor_char_boundary, hostdo_child_env,
        resolve_host_cwd_in_workspace, resolve_runner_container_cwd,
    };

    fn host_env(name: &str) -> Option<String> {
        match name {
            "PATH" => Some("/usr/bin:/bin".to_string()),
            "HOME" => Some("/Users/dev".to_string()),
            "LANG" => Some("en_US.UTF-8".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("hunter2".to_string()),
            "CARGO_TERM_COLOR" => Some("always".to_string()),
            "HARNESS_HAT_TOKEN" => Some("control-plane-secret".to_string()),
            _ => None,
        }
    }

    #[test]
    fn no_allowlist_means_inherit() {
        assert_eq!(hostdo_child_env(None, &host_env), None);
    }

    #[test]
    fn empty_allowlist_grants_only_the_base_set() {
        let vars = hostdo_child_env(Some(&[]), &host_env).expect("scrub applies");
        let names: Vec<&str> = vars.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["PATH", "HOME", "LANG"]);
        // Host secrets outside the base set must not leak through.
        assert!(!names.contains(&"AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn allowlist_adds_named_vars_but_never_control_plane_secrets() {
        let allowlist = [
            "CARGO_TERM_COLOR".to_string(),
            "HARNESS_HAT_TOKEN".to_string(),
            "MISSING_VAR".to_string(),
            "PATH".to_string(),
        ];
        let vars = hostdo_child_env(Some(&allowlist), &host_env).expect("scrub applies");
        let names: Vec<&str> = vars.iter().map(|(name, _)| name.as_str()).collect();
        // Allowlisted var present in the host env is passed through once.
        assert!(names.contains(&"CARGO_TERM_COLOR"));
        // HARNESS_HAT_* is stripped even when explicitly listed (M3).
        assert!(!names.contains(&"HARNESS_HAT_TOKEN"));
        // Unset host vars are skipped, duplicates of the base set deduped.
        assert!(!names.contains(&"MISSING_VAR"));
        assert_eq!(names.iter().filter(|n| **n == "PATH").count(), 1);
    }

    #[test]
    fn hostdo_cwd_maps_posix_container_paths_to_host_workspace() {
        let root = tempfile::tempdir().expect("temp workspace");
        let nested = root.path().join("src");
        std::fs::create_dir_all(&nested).expect("nested dir");

        let host_cwd =
            resolve_host_cwd_in_workspace("/workspace/src", Some("/workspace"), root.path())
                .expect("host cwd");
        assert_eq!(host_cwd, nested.canonicalize().expect("canonical nested"));

        let runner_cwd =
            resolve_runner_container_cwd("/workspace/src", &host_cwd, "/workspace", root.path());
        assert_eq!(runner_cwd, "/workspace/src");
    }

    #[test]
    fn hostdo_cwd_normalizes_windows_style_mount_identity() {
        let root = tempfile::tempdir().expect("temp workspace");
        let nested = root.path().join("src");
        std::fs::create_dir_all(&nested).expect("nested dir");

        let host_cwd =
            resolve_host_cwd_in_workspace("\\workspace\\src", Some("\\workspace"), root.path())
                .expect("host cwd");
        assert_eq!(host_cwd, nested.canonicalize().expect("canonical nested"));
    }

    #[test]
    fn captured_output_stays_bounded_on_utf8_and_invalid_bytes() {
        let mut output = "a".repeat(EXEC_JOB_CAPTURE_BYTES - 1);
        append_captured_bytes(&mut output, "🙂".as_bytes());
        append_captured_bytes(&mut output, &[0xff, 0xfe]);
        assert!(output.len() <= EXEC_JOB_CAPTURE_BYTES);
        assert!(output.is_char_boundary(output.len()));
    }

    #[test]
    fn floor_boundary_never_splits_a_multibyte_character() {
        assert_eq!(floor_char_boundary("a🙂b", 3), 1);
        assert_eq!(floor_char_boundary("a🙂b", 5), 5);
    }
}
