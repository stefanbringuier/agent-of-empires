//! Send-message, paste-image, and read-output endpoints.

use super::*;

// ============================================================================
// Send + read-output endpoints
//
// Together these are the minimum primitive an external orchestrator needs to
// run an aoe session as a controlled subagent: push a prompt in, read the
// pane back. Mirrors what the TUI's send-message dialog and pane preview do,
// without requiring keyboard or websocket attach.
// ============================================================================

#[derive(Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
    /// Whether to auto-revive a dead/stopped session before sending. Defaults
    /// to `true`; set to `false` for fail-loud behavior (parity with the
    /// `--no-revive` CLI flag).
    #[serde(default = "default_revive")]
    pub revive: bool,
}

fn default_revive() -> bool {
    true
}

enum SendKeysError {
    NotRunning,
    ResumeFailed(String),
    Transient(Status),
    StructuredView,
    Tmux(anyhow::Error),
    Ineligible,
}

type SendKeysResult =
    Result<(EnsureReadyOutcome, Instance), Box<(Instance, EnsureReadyOutcome, SendKeysError)>>;

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SendMessageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    send_message_inner(state, id, req, None).await
}

async fn send_message_inner(
    state: Arc<AppState>,
    id: String,
    req: Result<Json<SendMessageRequest>, axum::extract::rejection::JsonRejection>,
    source_session_id: Option<&str>,
) -> axum::response::Response {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    // Terminal keystroke injection: CityHall sessions are structured-view only
    // (the composer drives the agent via the ACP prompt route), so close this
    // explicitly rather than leaning on the downstream StructuredView error.
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(req) => req,
        Err(rejection) => return rejection.into_response(),
    };
    if req.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "message_empty"})),
        )
            .into_response();
    }

    // Serialize concurrent sends (and other tmux mutations) for this id.
    // Without this, two POSTs racing against the same session would issue
    // overlapping `tmux send-keys -l` invocations and the bytes can interleave
    // inside the pane.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    if let Some(source) = source_session_id {
        if let Err(message) = authorize_message(&state.profile, &instances, source, &id) {
            return (StatusCode::CONFLICT, message).into_response();
        }
    }
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);
    if source_session_id.is_some() && instance.is_structured() {
        return (
            StatusCode::CONFLICT,
            "Recipient view changed; select it again",
        )
            .into_response();
    }

    let sync_base = instance.clone();
    let tool = instance.tool.clone();
    let message = req.message;
    let revive = req.revive;
    let guarded_message = source_session_id.is_some();
    let send_result = tokio::task::spawn_blocking(move || -> SendKeysResult {
        // Revive the pane before sending. Without this, a send to a dead
        // pane silently writes keystrokes to a corpse with no agent.
        // Skipped when the caller opts out via `revive: false`.
        //
        // The closure surfaces both `inst_owned` AND the
        // `EnsureReadyOutcome` on the Err arm so the caller can sync
        // post-resume-path mutations (`agent_session_id`, failure marker,
        // and `retroactive_capture_excludes`) back to live state regardless
        // of which failure path fires. The
        // outcome lets the caller distinguish cascade-fired
        // (`Respawned`/`Started`) from the no-op `AlreadyAlive` path
        // so a sync only happens when there's actual cascade state to
        // propagate; this avoids clobbering live `last_error` on the
        // `revive=false + NotRunning` path where `started` is
        // unmutated.
        let mut inst_owned = instance;
        let outcome = if revive {
            match inst_owned.ensure_pane_ready() {
                Ok(o) => o,
                Err(e) => {
                    let mapped = match e {
                        EnsureReadyError::Transient(s) => SendKeysError::Transient(s),
                        EnsureReadyError::StructuredView => SendKeysError::StructuredView,
                        EnsureReadyError::Tmux(e) => SendKeysError::Tmux(e),
                    };
                    // ensure_pane_ready did not mutate user-visible
                    // state via the outcome path. Tag as AlreadyAlive
                    // so the outer match's `did_work` flag stays
                    // false. `EnsureReadyError::Tmux` may be either
                    // pre-cascade (tmux_session() / start_with_size
                    // subprocess failure: `inst_owned` unmutated) or
                    // post-resume-path (mutations committed).
                    // The Tmux outer arm syncs unconditionally and
                    // covers both shapes; the others (Transient /
                    // StructuredView) bail before any mutation.
                    return Err(Box::new((
                        inst_owned,
                        EnsureReadyOutcome::AlreadyAlive,
                        mapped,
                    )));
                }
            }
        } else {
            EnsureReadyOutcome::AlreadyAlive
        };
        if let EnsureReadyOutcome::ResumeFailed { sid } = &outcome {
            return Err(Box::new((
                inst_owned,
                outcome.clone(),
                SendKeysError::ResumeFailed(sid.clone()),
            )));
        }
        let tmux_session = match inst_owned.tmux_session() {
            Ok(s) => s,
            Err(e) => return Err(Box::new((inst_owned, outcome, SendKeysError::Tmux(e)))),
        };
        if !tmux_session.exists() {
            return Err(Box::new((inst_owned, outcome, SendKeysError::NotRunning)));
        }
        if guarded_message {
            let pane = crate::tmux::batch_pane_metadata()
                .ok()
                .and_then(|mut panes| panes.remove(tmux_session.name()));
            let Some(pane) = pane else {
                return Err(Box::new((inst_owned, outcome, SendKeysError::Ineligible)));
            };
            inst_owned.update_status_once(Some(&pane), Some(tmux_session.name()));
            if !message_target_eligible(&inst_owned)
                || pane.pane_dead
                || pane.pane_current_command.as_deref().is_none_or(|command| {
                    crate::tmux::utils::is_pane_running_shell_command(
                        command,
                        pane.pane_start_command_is_protected,
                    )
                })
            {
                return Err(Box::new((inst_owned, outcome, SendKeysError::Ineligible)));
            }
        }
        let delay = crate::agents::send_keys_enter_delay(&tool);
        if let Err(e) = tmux_session.send_keys_with_delay(&message, delay) {
            return Err(Box::new((inst_owned, outcome, SendKeysError::Tmux(e))));
        }
        Ok((outcome, inst_owned))
    })
    .await;

    match send_result {
        Ok(Ok((outcome, started))) => {
            // ensure_pane_ready mutated `started` (status, agent_session_id,
            // last_start_time, last_error) on the clone. Sync those back to
            // the live entry so the next request sees a coherent view;
            // without this, a rapid follow-up could generate a fresh
            // `agent_session_id` and orphan the prior Claude conversation.
            // See `apply_post_restart_sync`. Also stamp last_accessed_at so
            // the activity column reflects API-driven interaction.
            let mut instances = state.instances.write().await;
            let profile = if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                if !matches!(outcome, EnsureReadyOutcome::AlreadyAlive) {
                    apply_post_restart_sync(i, &sync_base, &started);
                }
                i.touch_last_accessed();
                i.source_profile.clone()
            } else {
                // Session was deleted between the send and the stamp; nothing
                // left to persist.
                return (StatusCode::OK, Json(serde_json::json!({"sent": true}))).into_response();
            };
            drop(instances);
            let id_for_save = id.clone();
            let sync_base_for_save = sync_base.clone();
            let started_for_save = started.clone();
            let outcome_already_alive = matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            tokio::task::spawn_blocking(move || {
                if let Ok(storage) = Storage::new(&profile, state.file_watch.clone()) {
                    if let Err(e) = storage.update(|all, _groups| {
                        if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id_for_save) {
                            if !outcome_already_alive {
                                apply_post_restart_sync(
                                    disk_inst,
                                    &sync_base_for_save,
                                    &started_for_save,
                                );
                            }
                            disk_inst.touch_last_accessed();
                        }
                        Ok(())
                    }) {
                        tracing::warn!(target: "http.api.sessions", "send_message: persist failed: {e}");
                    }
                }
            });
            (StatusCode::OK, Json(serde_json::json!({"sent": true}))).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, outcome, send_err) = *boxed;
            // ensure_pane_ready did mutate state when the outcome is
            // anything other than AlreadyAlive. `Started` and `Respawned`
            // touch fields the live entry needs to reflect (fresh sid from
            // acquire, last_start_time, etc.). Sync only when work happened.
            let did_work = !matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            match send_err {
                SendKeysError::Ineligible => (
                    StatusCode::CONFLICT,
                    "Recipient is busy, stopped, or no longer running a supported agent",
                )
                    .into_response(),
                SendKeysError::NotRunning => {
                    // External kill or remain-on-exit-off crash can race
                    // ensure_pane_ready's Alive decision against the
                    // tmux_session.exists() check. Propagate resume-path
                    // state when applicable; use the narrow sync helper to
                    // leave status and last_error untouched (NotRunning is
                    // recoverable; `started.status = Starting` from
                    // finalize_launch would briefly mis-paint a broken pane).
                    if did_work {
                        let mut instances = state.instances.write().await;
                        if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                            apply_cascade_state_sync(i, &sync_base, &started);
                        }
                    }
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"error": "session_not_running"})),
                    )
                        .into_response()
                }
                SendKeysError::ResumeFailed(sid) => {
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        apply_post_restart_sync(i, &sync_base, &started);
                    }
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": "resume_failed",
                            "message": format!("Resume failed for sid {sid}; preserved for explicit retry"),
                            "resume_session_id": sid,
                        })),
                    )
                        .into_response()
                }
                SendKeysError::Transient(status) => (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "session_transient",
                        "status": format!("{status:?}"),
                    })),
                )
                    .into_response(),
                SendKeysError::StructuredView => (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "acp_mode_unsupported"})),
                )
                    .into_response(),
                SendKeysError::Tmux(e) => {
                    tracing::error!(target: "http.api.sessions", "send_message: tmux error for {id}: {e}");
                    let msg = e.to_string();
                    // Sync cascade-mutated fields back to live state. Mirror
                    // `ensure_session`'s Err arm: full sync, then override
                    // `status` and `last_error` so observers don't see
                    // `Status::Starting` (set by `finalize_launch`) on a
                    // broken session. Tmux Err is the
                    // catch-all for both pre-cascade tmux failures (where
                    // `started` is unmutated and the sync is a no-op) and
                    // post-resume-path failures (where durable resume state
                    // must be copied back from the clone).
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        if apply_post_restart_sync(i, &sync_base, &started) {
                            i.status = crate::session::Status::Error;
                            i.last_error = Some(msg);
                        }
                    }
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "tmux_error"})),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "send_message: blocking task panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct MessageTargetsQuery {
    pub source_session_id: String,
}

#[derive(Deserialize)]
pub struct SessionMessageRequest {
    pub source_session_id: String,
    pub text: String,
}

fn message_target_eligible(instance: &Instance) -> bool {
    !instance.is_trashed()
        && !instance.is_archived()
        && !instance.is_snoozed()
        && if instance.is_structured() {
            matches!(
                instance.status,
                Status::Idle | Status::Waiting | Status::Running
            )
        } else {
            crate::agents::get_agent(&instance.tool).is_some()
                && matches!(instance.status, Status::Idle | Status::Waiting)
        }
}

fn message_source_authorized(profile: &str, instances: &[Instance], source: &str) -> bool {
    instances.iter().any(|instance| {
        instance.id == source
            && instance.effective_profile() == profile
            && crate::plugin::read_bridge::chat_owner(instance).is_some()
    })
}

pub(crate) fn authorize_message(
    profile: &str,
    instances: &[Instance],
    source: &str,
    target: &str,
) -> Result<(), &'static str> {
    if !message_source_authorized(profile, instances, source) {
        return Err("Source chat is unavailable or unauthorized in this profile");
    }
    if source == target {
        return Err("Select a different recipient");
    }
    let target = instances
        .iter()
        .find(|instance| instance.id == target)
        .ok_or("Recipient no longer exists")?;
    if target.effective_profile() != profile || !message_target_eligible(target) {
        return Err("Recipient is unavailable, busy, or unauthorized in this profile");
    }
    Ok(())
}

pub async fn message_targets(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<MessageTargetsQuery>,
) -> axum::response::Response {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = crate::server::api::cityhall_block(&state) {
        return response;
    }
    let instances = state.instances.read().await;
    if !message_source_authorized(&state.profile, &instances, &query.source_session_id) {
        return (
            StatusCode::FORBIDDEN,
            "Source chat is unavailable or unauthorized",
        )
            .into_response();
    }
    let targets: Vec<_> = instances
        .iter()
        .filter(|instance| {
            instance.id != query.source_session_id
                && instance.effective_profile() == state.profile
                && message_target_eligible(instance)
        })
        .cloned()
        .collect();
    drop(instances);
    match tokio::task::spawn_blocking(move || {
        let fullscreen = crate::claude_settings::read_tui_fullscreen();
        let panes = crate::tmux::batch_pane_metadata().unwrap_or_default();
        targets
            .into_iter()
            .filter_map(|instance| {
                if !instance.is_structured() {
                    let tmux = instance.tmux_session().ok()?;
                    let pane = panes.get(tmux.name())?;
                    if pane.pane_dead
                        || pane.pane_current_command.as_deref().is_none_or(|command| {
                            crate::tmux::utils::is_pane_running_shell_command(
                                command,
                                pane.pane_start_command_is_protected,
                            )
                        })
                    {
                        return None;
                    }
                }
                Some(SessionResponse::from_instance(&instance, fullscreen))
            })
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(sessions) => Json(serde_json::json!({"sessions": sessions})).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Recipient inventory unavailable",
        )
            .into_response(),
    }
}

pub async fn submit_session_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SessionMessageRequest>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    if state.read_only || state.cityhall_mode {
        return message_result("error", "Cross-session messaging is disabled in this view");
    }
    let Json(req) = match req {
        Ok(req) => req,
        Err(rejection) => return message_result("error", &rejection.to_string()),
    };
    if req.text.trim().is_empty() {
        return message_result("error", "Message is empty");
    }
    let structured = {
        let instances = state.instances.read().await;
        if let Err(message) =
            authorize_message(&state.profile, &instances, &req.source_session_id, &id)
        {
            return message_result("error", message);
        }
        instances
            .iter()
            .any(|instance| instance.id == id && instance.is_structured())
    };
    let response = if structured {
        super::super::acp::acp_prompt_inner(
            state,
            id,
            Ok(Json(crate::acp::protocol::PromptRequest {
                text: req.text,
                attachments: Vec::new(),
                prompt_id: None,
            })),
            Some(&req.source_session_id),
        )
        .await
    } else {
        send_message_inner(
            state,
            id,
            Ok(Json(SendMessageRequest {
                message: req.text,
                revive: false,
            })),
            Some(&req.source_session_id),
        )
        .await
    };
    message_acknowledgment(response).await
}

async fn message_acknowledgment(response: axum::response::Response) -> axum::response::Response {
    let code = response.status();
    let body = match axum::body::to_bytes(response.into_body(), 16_384).await {
        Ok(body) => body,
        Err(_) => {
            return message_result(
                "unknown",
                "Acknowledgment unavailable; verify recipient before retrying",
            );
        }
    };
    let body = String::from_utf8_lossy(&body);
    if code.is_success() {
        let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let status = value
            .get("disposition")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                (value.get("sent").and_then(serde_json::Value::as_bool) == Some(true))
                    .then_some("sent")
            });
        return match status {
            Some(status @ ("sent" | "steered" | "queued")) => {
                message_result(status, "Input accepted; execution is not confirmed")
            }
            _ => message_result(
                "unknown",
                "Acknowledgment unavailable; verify recipient before retrying",
            ),
        };
    }
    let status = if code.is_client_error()
        || body.starts_with("worker_not_ready")
        || body.starts_with("worker_capacity_full")
    {
        "error"
    } else {
        "unknown"
    };
    message_result(status, &body)
}

fn message_result(status: &str, message: &str) -> axum::response::Response {
    Json(serde_json::json!({"status": status, "message": message})).into_response()
}

/// Max decoded size of a pasted image (5 MiB). Claude Code caps image
/// attachments around this size; the route body limit in `build_router`
/// leaves headroom for base64's ~33% overhead plus JSON framing.
const MAX_PASTE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Directory, relative to the session worktree, holding images pasted into
/// the live terminal. It lives inside the worktree so a Docker-sandboxed
/// pane, which mounts the worktree but cannot see the host temp dir, can
/// still read the file. A self-ignoring `.gitignore` keeps the blobs out of
/// git. See #2678.
const PASTE_IMAGE_DIR: &str = ".aoe-pasted-images";

#[derive(Deserialize)]
pub struct PasteImageRequest {
    /// Client-declared MIME. Advisory only: the extension and the
    /// accept/reject decision come from magic-byte sniffing, never this field.
    #[serde(default)]
    pub mime_type: String,
    /// Standard-base64 image bytes.
    pub data: String,
}

fn paste_image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Write the decoded blob into the worktree's paste-image dir and return the
/// host path plus the generated file name. Sync (filesystem I/O); call from a
/// blocking pool.
fn write_paste_image(
    project_path: &str,
    bytes: &[u8],
    ext: &str,
) -> std::io::Result<(std::path::PathBuf, String)> {
    let dir = std::path::Path::new(project_path).join(PASTE_IMAGE_DIR);
    std::fs::create_dir_all(&dir)?;
    // A `.gitignore` of `*` also ignores itself, so the whole directory stays
    // invisible to `git add` with no git subprocess.
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")?;
    }
    let file_name = format!("aoe-paste-{}.{}", uuid::Uuid::new_v4(), ext);
    let path = dir.join(&file_name);
    // create_new: uuid names never collide; fail loud if the impossible happens.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::io::Write::write_all(&mut f, bytes)?;
    Ok((path, file_name))
}

/// Map the host paste-image file to the path the tmux pane reads. Non-sandboxed
/// panes share the host filesystem, so the absolute host path is correct. A
/// sandboxed pane mounts the worktree under a container path (`/workspace/...`);
/// reuse `compute_volume_paths` so the pasted path matches that mount.
fn pane_visible_paste_path(project_path: &str, is_sandboxed: bool, file_name: &str) -> String {
    if is_sandboxed {
        if let Ok((_, working_dir)) = crate::session::config::container_config::compute_volume_paths(
            std::path::Path::new(project_path),
            project_path,
        ) {
            return format!("{working_dir}/{PASTE_IMAGE_DIR}/{file_name}");
        }
    }
    std::path::Path::new(project_path)
        .join(PASTE_IMAGE_DIR)
        .join(file_name)
        .to_string_lossy()
        .to_string()
}

/// Save a clipboard image pasted into the live terminal and return the path
/// the tmux pane can read, so the CLI agent (e.g. Claude Code) attaches it.
/// See #2678.
pub async fn paste_image(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<PasteImageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    use base64::Engine as _;

    if state.read_only {
        return crate::server::api::read_only_response();
    }
    // Allowed for the CityHall composer, but only against a structured
    // session: a plain/terminal target would let a locked-down client write
    // into another session's worktree.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    let bytes = match base64::engine::general_purpose::STANDARD.decode(req.data.as_bytes()) {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_base64"})),
            )
                .into_response();
        }
    };
    if bytes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "empty"})),
        )
            .into_response();
    }
    if bytes.len() > MAX_PASTE_IMAGE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "too_large"})),
        )
            .into_response();
    }
    let Some(mime) = crate::server::api::acp::sniff_image_mime(&bytes) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "not_an_image"})),
        )
            .into_response();
    };
    let ext = paste_image_extension(mime);

    let project_path = instance.project_path.clone();
    let is_sandboxed = instance.is_sandboxed();
    let write_project = project_path.clone();
    let (host_path, file_name) =
        match tokio::task::spawn_blocking(move || write_paste_image(&write_project, &bytes, ext))
            .await
        {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => {
                tracing::warn!(target: "http.api.sessions", "paste_image: write failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "write_failed"})),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::warn!(target: "http.api.sessions", "paste_image: join failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "write_failed"})),
                )
                    .into_response();
            }
        };

    // Best-effort TTL cleanup: the file only needs to outlive the agent
    // reading it. A detached task keeps the worktree from accumulating blobs
    // without any teardown bookkeeping.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        let _ = tokio::fs::remove_file(&host_path).await;
    });

    let pane_path = pane_visible_paste_path(&project_path, is_sandboxed, &file_name);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "path": pane_path })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct OutputQuery {
    #[serde(default = "default_output_lines")]
    pub lines: u32,
    #[serde(default = "default_output_format")]
    pub format: String,
}

fn default_output_lines() -> u32 {
    200
}

fn default_output_format() -> String {
    "text".to_string()
}

enum CaptureError {
    NotRunning,
    Tmux(anyhow::Error),
}

pub async fn read_output(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<OutputQuery>,
) -> impl IntoResponse {
    // Raw terminal pane content: CityHall hides the terminal UI + WS relay, so
    // this read must be closed too or the pane is reachable by session id.
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let lines = (q.lines as usize).clamp(1, 2000);
    let want_ansi = match q.format.as_str() {
        "ansi" => true,
        "text" => false,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "format_invalid",
                    "allowed": ["text", "ansi"]
                })),
            )
                .into_response();
        }
    };

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    let capture_result = tokio::task::spawn_blocking(move || -> Result<String, CaptureError> {
        let tmux_session = instance.tmux_session().map_err(CaptureError::Tmux)?;
        if !tmux_session.exists() {
            return Err(CaptureError::NotRunning);
        }
        let raw = tmux_session
            .capture_pane(lines)
            .map_err(CaptureError::Tmux)?;
        if want_ansi {
            Ok(raw)
        } else {
            Ok(crate::tmux::utils::strip_ansi(&raw))
        }
    })
    .await;

    match capture_result {
        Ok(Ok(content)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": id,
                "lines": lines,
                "format": q.format,
                "content": content,
            })),
        )
            .into_response(),
        Ok(Err(CaptureError::NotRunning)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "session_not_running"})),
        )
            .into_response(),
        Ok(Err(CaptureError::Tmux(e))) => {
            tracing::error!(target: "http.api.sessions", "read_output: tmux error for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "tmux_error"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "read_output: blocking task panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod send_output_tests {
    use super::*;

    #[test]
    fn messaging_targets_preserve_lifecycle_and_supported_agent_boundaries() {
        for (view, tool, status, accepted) in [
            (crate::session::View::Terminal, "claude", Status::Idle, true),
            (
                crate::session::View::Terminal,
                "claude",
                Status::Waiting,
                true,
            ),
            (
                crate::session::View::Terminal,
                "claude",
                Status::Running,
                false,
            ),
            (crate::session::View::Terminal, "shell", Status::Idle, false),
            (
                crate::session::View::Structured,
                "claude",
                Status::Running,
                true,
            ),
            (
                crate::session::View::Structured,
                "claude",
                Status::Stopped,
                false,
            ),
            (
                crate::session::View::Structured,
                "claude",
                Status::Error,
                false,
            ),
        ] {
            let mut instance = Instance::new("duplicate title", "/repo");
            instance.view = view;
            instance.tool = tool.into();
            instance.status = status;
            assert_eq!(
                message_target_eligible(&instance),
                accepted,
                "{view:?} {tool} {status:?}"
            );
            instance.archived_at = Some(chrono::Utc::now());
            assert!(!message_target_eligible(&instance));
            instance.archived_at = None;
            instance.trashed_at = Some(chrono::Utc::now());
            assert!(!message_target_eligible(&instance));
            instance.trashed_at = None;
            instance.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::hours(1));
            assert!(!message_target_eligible(&instance));
        }
    }

    #[tokio::test]
    async fn messaging_denied_source_does_not_reach_submission_or_touch_recipient() {
        let mut source = Instance::new("duplicate title", "/source");
        source.source_profile = "test".into();
        source.view = crate::session::View::Structured;
        let mut target = Instance::new("duplicate title", "/target");
        target.source_profile = "test".into();
        let state =
            crate::server::test_support::build_test_app_state(vec![source.clone(), target.clone()]);
        let response = submit_session_message(
            State(state.clone()),
            Path(target.id.clone()),
            Ok(Json(SessionMessageRequest {
                source_session_id: source.id,
                text: " exact message\n ".into(),
            })),
        )
        .await;
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "error");
        assert!(body["message"].as_str().unwrap().contains("unauthorized"));
        assert!(state.instance_locks.read().await.is_empty());
        let instances = state.instances.read().await;
        let recipient = instances
            .iter()
            .find(|instance| instance.id == target.id)
            .unwrap();
        assert_eq!(recipient.last_accessed_at, target.last_accessed_at);
        assert_eq!(recipient.status, target.status);
    }

    #[tokio::test]
    async fn messaging_reports_acknowledgment_without_promising_execution() {
        for (code, body, expected) in [
            (StatusCode::OK, r#"{"sent":true}"#, "sent"),
            (
                StatusCode::ACCEPTED,
                r#"{"disposition":"queued","queued_id":"q"}"#,
                "queued",
            ),
            (
                StatusCode::ACCEPTED,
                r#"{"disposition":"steered"}"#,
                "steered",
            ),
            (StatusCode::ACCEPTED, "", "unknown"),
            (StatusCode::CONFLICT, "recipient stopped", "error"),
            (StatusCode::SERVICE_UNAVAILABLE, "worker_not_ready", "error"),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "lost acknowledgment",
                "unknown",
            ),
        ] {
            let response = message_acknowledgment((code, body).into_response()).await;
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(result["status"], expected, "{code} {body}");
        }
    }

    #[test]
    fn output_query_default_constants() {
        assert_eq!(default_output_lines(), 200);
        assert_eq!(default_output_format(), "text");
    }

    #[test]
    fn send_message_request_requires_message_field() {
        let r: Result<SendMessageRequest, _> = serde_json::from_str("{}");
        assert!(r.is_err(), "missing message must reject");
    }

    #[test]
    fn send_message_request_accepts_message() {
        let r: SendMessageRequest = serde_json::from_str("{\"message\":\"hello\"}").unwrap();
        assert_eq!(r.message, "hello");
    }
}

#[cfg(test)]
mod paste_image_tests {
    use super::*;
    use tempfile::tempdir;

    const PNG_1PX: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    #[test]
    fn extension_from_sniffed_mime() {
        assert_eq!(paste_image_extension("image/png"), "png");
        assert_eq!(paste_image_extension("image/jpeg"), "jpg");
        assert_eq!(paste_image_extension("image/gif"), "gif");
        assert_eq!(paste_image_extension("image/webp"), "webp");
    }

    #[test]
    fn write_paste_image_lands_in_worktree_and_ignores_itself() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let (path, name) = write_paste_image(&project, PNG_1PX, "png").unwrap();

        assert!(path.exists(), "image file must be written");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            PNG_1PX,
            "bytes must round-trip"
        );
        assert!(name.starts_with("aoe-paste-") && name.ends_with(".png"));
        let gitignore = dir.path().join(PASTE_IMAGE_DIR).join(".gitignore");
        assert_eq!(
            std::fs::read_to_string(gitignore).unwrap(),
            "*\n",
            "dir must self-ignore so pasted blobs never reach git"
        );
    }

    #[test]
    fn non_sandboxed_pane_path_is_absolute_host_path() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let pane = pane_visible_paste_path(&project, false, "aoe-paste-x.png");

        let expected = dir
            .path()
            .join(PASTE_IMAGE_DIR)
            .join("aoe-paste-x.png")
            .to_string_lossy()
            .to_string();
        assert_eq!(pane, expected);
    }

    #[test]
    fn sandboxed_pane_path_uses_container_mount() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let dir_name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let pane = pane_visible_paste_path(&project, true, "aoe-paste-x.png");

        // A non-git worktree mounts under /workspace/<dir-name>; the pasted
        // path must be the container-visible path, not the host path.
        assert_eq!(
            pane,
            format!("/workspace/{dir_name}/{PASTE_IMAGE_DIR}/aoe-paste-x.png")
        );
    }
}
