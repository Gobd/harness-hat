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
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::{mpsc, oneshot};

use crate::activity::{Activity, ActivityEvent, ActivityKind, ActivityState};
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
const EXEC_JOB_CAPTURE_BYTES: usize = 1024 * 1024;

pub(super) async fn exec_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(req): Json<ExecRequest>,
) -> Response {
    let supports_jobs = supports_exec_jobs(&headers);
    let wants_detached = req.detach || supports_jobs;

    let (session_token, identity) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };

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
        .find(|workspace| workspace.name == identity.project)
    else {
        return deny_with_audit(
            &state,
            &identity.project,
            &req.argv,
            &req.cwd,
            "unknown workspace",
        )
        .await;
    };

    let request_cwd = PathBuf::from(&req.cwd);
    let host_cwd = match resolve_host_cwd_in_workspace(
        &request_cwd,
        Some(identity.mount_target.as_str()),
        &project.canonical_path,
    ) {
        Ok(path) => path,
        Err(reason) => {
            return deny_with_audit(&state, &identity.project, &req.argv, &req.cwd, reason).await;
        }
    };

    let rules = match crate::config::load_composed_rules_for_workspace(
        &cfg,
        Some(identity.project.as_str()),
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

    let matched_rule = rules.find_hostdo(&req.argv, req.image.as_deref());
    let matched_command = matched_rule.and_then(|entry| entry.name.clone());
    let approval_mode = matched_rule
        .map(|entry| entry.approval_mode)
        .unwrap_or(rules.hostdo.default_policy);
    if approval_mode == NetworkPolicy::Deny {
        return deny_with_audit(
            &state,
            &identity.project,
            &req.argv,
            &req.cwd,
            "command denied by hostdo policy",
        )
        .await;
    }

    let timeout_secs = req
        .timeout_secs
        .or_else(|| matched_rule.map(|entry| entry.timeout_secs))
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .max(1);
    let runner_mount_target = PathBuf::from(&identity.mount_target);
    let runner_cwd = resolve_runner_container_cwd(
        &request_cwd,
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
        identity.project.clone(),
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
        project: identity.project.clone(),
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
        project: identity.project.clone(),
        container_id: Some(identity.container_id.clone()),
        argv: req.argv.clone(),
        image: req.image.clone(),
        timeout_secs,
        cwd: host_cwd.clone(),
        rule_cwd: request_cwd,
        matched_command,
        response_tx: Some(response_tx),
    };
    if state.pending_tx.send(pending).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "manager_shutting_down".into(),
                reason: "manager is shutting down".into(),
            }),
        )
            .into_response();
    }

    if wants_detached {
        return start_approval_wait_job(run, response_rx);
    }

    await_approval_and_execute(run, response_rx).await
}

pub(super) async fn exec_jobs_list_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    let (_, identity) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    Json(ExecJobListResponse {
        jobs: state.exec_jobs.list_for_project(&identity.project),
    })
    .into_response()
}

pub(super) async fn exec_job_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    let (_, identity) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let Some(job) = state.exec_jobs.get_for_project(&job_id, &identity.project) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".into(),
                reason: "unknown exec job".into(),
            }),
        )
            .into_response();
    };
    Json(job).into_response()
}

pub(super) async fn exec_job_output_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
    Query(query): Query<ExecJobOutputQuery>,
) -> Response {
    let (_, identity) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let Some(job) = state.exec_jobs.get_for_project(&job_id, &identity.project) else {
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
    let (_, identity) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let Some(job) = state.exec_jobs.get_for_project(&job_id, &identity.project) else {
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
    let (_, identity) = match require_session_context(&state, &headers) {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let Some(job) = state.exec_jobs.get_for_project(&job_id, &identity.project) else {
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
    if stdin_tx.send(req.input.into_bytes()).is_err() {
        return (
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "not_running".into(),
                reason: "job input channel is closed".into(),
            }),
        )
            .into_response();
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
    project: String,
    container_id: String,
    argv: Vec<String>,
    image: Option<String>,
    host_cwd: PathBuf,
    request_cwd: String,
    timeout_secs: u64,
    workspace_path: PathBuf,
    mount_target: PathBuf,
    runner_cwd: PathBuf,
    activity_id: String,
    cancel_flag: Arc<AtomicBool>,
    decision_kind: DecisionKind,
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
                    &run.project,
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
                &run.project,
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
            if remember {
                remember_hostdo_command(
                    &run.state,
                    &run.project,
                    &run.argv,
                    run.image.as_deref(),
                    run.timeout_secs,
                );
            }
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
) -> Response {
    let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
    let status = run.state.exec_jobs.insert(ExecJobStatus {
        state: ExecJobState::Running,
        job_id: String::new(),
        project: run.project.clone(),
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
    });
    let job_id = status.job_id.clone();
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
                        &run_clone.project,
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
                    &run_clone.project,
                    &run_clone.argv,
                    &run_clone.request_cwd,
                    DecisionKind::Denied,
                    None,
                    None,
                )
                .await;
            }
            ApprovalDecision::Approve { remember } => {
                if remember {
                    remember_hostdo_command(
                        &run_clone.state,
                        &run_clone.project,
                        &run_clone.argv,
                        run_clone.image.as_deref(),
                        run_clone.timeout_secs,
                    );
                }
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
    let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
    let status = run.state.exec_jobs.insert(ExecJobStatus {
        state: ExecJobState::Running,
        job_id: String::new(),
        project: run.project.clone(),
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
    });
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
    stdin_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    match run_command(run, Some((job_id.clone(), stdin_rx))).await {
        Ok(_) => {}
        Err(_) => {}
    }
}

async fn run_command(
    run: CommandRun,
    job: Option<(String, mpsc::UnboundedReceiver<Vec<u8>>)>,
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
                &run.project,
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
        &run.project,
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
        run.state.exec_jobs.update(&job_id, |status| {
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

    let status = match wait_for_child(&mut child, run.cancel_flag.clone(), IMAGE_PULL_TIMEOUT_SECS).await {
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
        let mut cmd = TokioCommand::new("docker");
        cmd.arg("run")
            .arg("--rm")
            .arg("-i")
            .arg("-v")
            .arg(format!(
                "{}:{}",
                run.workspace_path.display(),
                run.mount_target.display()
            ))
            .arg("-w")
            .arg(run.runner_cwd.display().to_string())
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
    // Place each spawned process in its own process group so kill_child_and_group
    // can reach all descendants, not just the direct child.
    #[cfg(unix)]
    cmd.process_group(0);
    Ok(cmd)
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
    reader: R,
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
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| ExecFailure::Message(format!("failed reading process output: {e}")))?
        {
            let prefix = if is_stderr { "stderr" } else { "stdout" };
            let _ = state.activity_tx.try_send(ActivityEvent::Line {
                id: activity_id.clone(),
                line: format!("{prefix}: {line}"),
            });
            append_captured_output(&mut buf, &line);
            if let Some(job_id) = &job_id {
                state.exec_jobs.update(job_id, |status| {
                    let output = if is_stderr {
                        &mut status.stderr
                    } else {
                        &mut status.stdout
                    };
                    append_job_output(output, &line);
                });
            }
        }
        Ok(buf)
    })
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
            project: project.to_string(),
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

pub(super) fn remember_hostdo_command(
    state: &ServerState,
    project: &str,
    command: &[String],
    image: Option<&str>,
    timeout_secs: u64,
) -> bool {
    let config = state.config.get();
    let Some(workspace) = config.workspaces.iter().find(|w| w.name == project) else {
        return false;
    };
    let rules_path = workspace.canonical_path.join("harness-rules.toml");
    crate::rules::append_hostdo_auto_approval(&rules_path, command, image, timeout_secs)
        .unwrap_or(false)
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
    request_cwd: &Path,
    mount_target: Option<&str>,
    workspace_host_path: &Path,
) -> Result<PathBuf, String> {
    let workspace_root = workspace_host_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_host_path.to_path_buf());
    if let Some(mount_target) = mount_target {
        let mount_target = Path::new(mount_target);
        if request_cwd == mount_target {
            return Ok(workspace_root);
        }
        if let Ok(rel) = request_cwd.strip_prefix(mount_target) {
            return confine_host_cwd_to_workspace(&workspace_root.join(rel), &workspace_root);
        }
    }
    confine_host_cwd_to_workspace(request_cwd, &workspace_root)
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
    request_cwd: &Path,
    host_cwd: &Path,
    mount_target: &Path,
    workspace_host_path: &Path,
) -> PathBuf {
    let workspace_root = workspace_host_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_host_path.to_path_buf());
    if host_cwd == workspace_root.as_path() || host_cwd.starts_with(&workspace_root) {
        if let Ok(rel) = host_cwd.strip_prefix(&workspace_root) {
            return mount_target.join(rel);
        }
    }
    if request_cwd.is_absolute() {
        request_cwd.to_path_buf()
    } else {
        mount_target.to_path_buf()
    }
}

fn append_captured_output(output: &mut String, line: &str) {
    if output.len() >= EXEC_JOB_CAPTURE_BYTES {
        return;
    }
    let remaining = EXEC_JOB_CAPTURE_BYTES - output.len();
    if line.len() + 1 <= remaining {
        output.push_str(line);
        output.push('\n');
    } else {
        output.push_str(&line[..remaining.min(line.len())]);
    }
}

fn append_job_output(output: &mut Option<String>, line: &str) {
    let buf = output.get_or_insert_with(String::new);
    append_captured_output(buf, line);
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
