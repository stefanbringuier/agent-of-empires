//! Bounded profile-scoped reads shared by plugin workers and session tools.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::acp::{event_store::EventStore, state::Event};
use crate::session::{Instance, Storage, View};

use super::host_api::DispatchError;
use super::protocol::codes;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Search {
    query: String,
    agent: Option<String>,
    group: Option<String>,
    status: Option<String>,
    include_archived: bool,
    exclude_session_id: Option<String>,
    after_id: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRead {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityRead {
    session_id: String,
    limit: Option<usize>,
    max_chars: Option<usize>,
}

pub(super) fn dispatch(
    storage: &Storage,
    events: Option<&EventStore>,
    method: &str,
    params: &Value,
) -> Result<Value, DispatchError> {
    let instances = storage
        .load()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    match method {
        "sessions.search" => search(&instances, parse(params)?),
        "sessions.details" => {
            let req: SessionRead = parse(params)?;
            let instance = resolve(&instances, &req.session_id)?;
            Ok(details(instance, events))
        }
        "sessions.recent_activity" => {
            let req: ActivityRead = parse(params)?;
            let lines = bounded(req.limit, 100, 200)?;
            let chars = bounded(req.max_chars, 16_000, 32_000)?;
            let instance = resolve(&instances, &req.session_id)?;
            Ok(activity(instance, events, lines, chars))
        }
        _ => Err(DispatchError::invalid_params("unknown session read")),
    }
}

fn parse<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, DispatchError> {
    serde_json::from_value(if params.is_null() {
        json!({})
    } else {
        params.clone()
    })
    .map_err(|e| DispatchError::invalid_params(e.to_string()))
}

fn bounded(value: Option<usize>, default: usize, maximum: usize) -> Result<usize, DispatchError> {
    match value.unwrap_or(default) {
        0 => Err(DispatchError::invalid_params("limit must be positive")),
        value => Ok(value.min(maximum)),
    }
}

fn resolve<'a>(instances: &'a [Instance], id: &str) -> Result<&'a Instance, DispatchError> {
    let instance = instances.iter().find(|i| i.id == id).ok_or_else(|| {
        DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "session_not_found",
            "session does not exist in the authorized profile",
        )
    })?;
    if instance.is_trashed() {
        return Err(DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "session_trashed",
            "session is in trash; restore it to read its conversation",
        ));
    }
    Ok(instance)
}

fn search(instances: &[Instance], req: Search) -> Result<Value, DispatchError> {
    let limit = bounded(req.limit, 20, 100)?;
    if req.query.chars().count() > 512 {
        return Err(DispatchError::invalid_params(
            "query exceeds 512 characters",
        ));
    }
    let query = req.query.to_lowercase();
    let mut matches: Vec<_> = instances
        .iter()
        .filter(|i| {
            !i.is_trashed()
                && (req.include_archived || !i.is_archived())
                && req.exclude_session_id.as_ref() != Some(&i.id)
                && req.after_id.as_ref().is_none_or(|after| i.id > *after)
                && req
                    .agent
                    .as_ref()
                    .is_none_or(|agent| session_agent(i) == agent.as_str())
                && req
                    .group
                    .as_ref()
                    .is_none_or(|group| i.group_path == *group)
                && req
                    .status
                    .as_ref()
                    .is_none_or(|status| format!("{:?}", i.status).eq_ignore_ascii_case(status))
                && [
                    i.title.as_str(),
                    i.id.as_str(),
                    i.project_path.as_str(),
                    session_agent(i),
                    i.group_path.as_str(),
                    &format!("{:?}", i.status),
                ]
                .iter()
                .any(|field| field.to_lowercase().contains(&query))
        })
        .collect();
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    let mut sessions = Vec::new();
    let mut characters = 0;
    let mut next_after_id = None;
    for instance in &matches {
        let row = metadata(instance);
        let size = row.to_string().chars().count();
        if sessions.len() == limit || characters + size > 16_000 {
            next_after_id = sessions
                .last()
                .and_then(|row: &Value| row["id"].as_str())
                .map(str::to_owned);
            break;
        }
        characters += size;
        sessions.push(row);
    }
    Ok(json!({
        "sessions": sessions,
        "next_after_id": next_after_id,
        "omitted": matches.len().saturating_sub(sessions.len()),
        "observed_at": chrono::Utc::now(),
        "source": "session_registry",
        "status_freshness": "stored_not_live",
    }))
}

fn text(value: &str) -> String {
    normalize(value).chars().take(512).collect()
}

fn session_agent(instance: &Instance) -> &str {
    if instance.is_structured() {
        instance
            .agent_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&instance.tool)
    } else {
        &instance.tool
    }
}

fn metadata(i: &Instance) -> Value {
    json!({
        "id": i.id,
        "title": text(&i.title),
        "agent": (!session_agent(i).is_empty()).then(|| text(session_agent(i))),
        "project_path": (!i.project_path.is_empty()).then(|| text(&i.project_path)),
        "group": text(&i.group_path),
        "status": i.status,
        "view": i.view,
        "archived": i.is_archived(),
        "snoozed": i.is_snoozed(),
        "metadata_truncated": [i.title.as_str(), session_agent(i), i.project_path.as_str(), i.group_path.as_str()]
            .iter().any(|field| field.chars().count() > 512),
    })
}

fn details(i: &Instance, events: Option<&EventStore>) -> Value {
    let mut result = metadata(i);
    let fields = json!({
        "created_at": i.created_at,
        "last_accessed_at": i.last_accessed_at,
        "idle_entered_at": i.idle_entered_at,
        "last_activity_at_ms": events.and_then(|store| {
            store.last_event_at_for_sessions(std::slice::from_ref(&i.id)).remove(&i.id)
        }),
        "archived_at": i.archived_at,
        "idle_dormant_since": i.idle_dormant_since,
        "lifecycle": i.lifecycle_reservation,
        "scratch": i.scratch,
        "branch": i.worktree_info.as_ref().map(|w| text(&w.branch)),
        "terminal_created": i.terminal_info.as_ref().map(|t| t.created),
        "worktree": i.worktree_info.as_ref().map(|w| json!({
            "branch": text(&w.branch),
            "main_repo_path": text(&w.main_repo_path),
            "managed_by_aoe": w.managed_by_aoe,
        })),
        "sandbox": i.sandbox_info.as_ref().map(|s| json!({
            "enabled": s.enabled,
            "container_id": s.container_id.as_deref().map(text),
            "image": text(&s.image),
            "container_name": text(&s.container_name),
        })),
        "observed_at": chrono::Utc::now(),
        "source": "session_registry",
        "status_freshness": "stored_not_live",
    });
    result
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    result
}

fn activity(i: &Instance, events: Option<&EventStore>, lines: usize, chars: usize) -> Value {
    let (source, capture, previous_truncation, observed_at_ms) = match i.view {
        View::Structured => {
            let Some(events) = events else {
                return unavailable(
                    i,
                    "structured_events",
                    "unavailable",
                    "event store unavailable",
                );
            };
            let page = events.replay_page_before(&i.id, u64::MAX, Some(200));
            let mut content = String::new();
            for (_, event) in page.events {
                match event {
                    Event::AgentMessageChunk { text } => content.push_str(&text),
                    Event::UserPromptSent { text, .. } => {
                        content.push_str("\n[user]\n");
                        content.push_str(&text);
                        content.push_str("\n[assistant]\n");
                    }
                    Event::ToolCallStarted { tool_call } => {
                        content.push_str("\n[tool]\n");
                        content.push_str(&tool_call.name);
                        content.push('\n');
                        content.push_str(&tool_call.args_preview);
                        content.push('\n');
                    }
                    Event::ToolCallCompleted {
                        content: output, ..
                    } => {
                        content.push_str("\n[tool output]\n");
                        content.push_str(&output);
                        content.push('\n');
                    }
                    _ => {}
                }
            }
            (
                "structured_events",
                Ok(content),
                page.has_more,
                events
                    .last_event_at_for_sessions(std::slice::from_ref(&i.id))
                    .remove(&i.id),
            )
        }
        View::Terminal => (
            "terminal_capture",
            i.tmux_session()
                .and_then(|session| session.capture_pane(lines + 1)),
            false,
            None,
        ),
    };
    match capture {
        Ok(content) if !content.is_empty() => {
            let (content, truncated) = recent_text(&content, lines, chars);
            json!({
                "session_id": i.id,
                "state": "available",
                "source": source,
                "captured_at": chrono::Utc::now(),
                "last_activity_at_ms": observed_at_ms,
                "text": content,
                "truncated": truncated || previous_truncation,
                "scope": "recent_window_only",
            })
        }
        Ok(_) => unavailable(i, source, "unavailable", "no recent text available"),
        Err(_) => unavailable(i, source, "error", "terminal capture failed"),
    }
}

fn unavailable(i: &Instance, source: &str, state: &str, reason: &str) -> Value {
    json!({
        "session_id": i.id,
        "state": state,
        "source": source,
        "captured_at": chrono::Utc::now(),
        "text": "",
        "truncated": false,
        "reason": reason,
    })
}

fn normalize(value: &str) -> String {
    static CONTROLS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let controls = CONTROLS.get_or_init(|| {
        regex::Regex::new(
            r"(?s)(?:\x1b\[|\x{009b})[0-?]*[ -/]*[@-~]|(?:\x1b\]|\x{009d}).*?(?:\x07|\x1b\\|\x{009c}|$)|\x1b[PX^_].*?(?:\x1b\\|$)|\x1b[ -/]*[@-~]",
        )
        .expect("terminal control sequence pattern")
    });
    let plain = controls.replace_all(value, "");
    let plain: String = plain
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    crate::acp::acp_client::scrub_stderr_secrets(&plain).into_owned()
}

fn recent_text(value: &str, lines: usize, chars: usize) -> (String, bool) {
    let plain = normalize(value);
    let all_lines: Vec<_> = plain.lines().collect();
    let start = all_lines.len().saturating_sub(lines);
    let window = all_lines[start..].join("\n");
    let skip = window.chars().count().saturating_sub(chars);
    (window.chars().skip(skip).collect(), start > 0 || skip > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_scopes_filters_and_pages_duplicate_titles() {
        let mut instances: Vec<_> = (0..4)
            .map(|n| {
                let mut instance = Instance::new("same", "/work/project");
                instance.id = format!("session-{n}");
                instance.tool = "claude".into();
                instance.group_path = "work".into();
                instance
            })
            .collect();
        instances[2].archived_at = Some(chrono::Utc::now());
        instances[3].trashed_at = Some(chrono::Utc::now());
        for query in ["same", "session", "project", "claude", "work"] {
            let first = search(
                &instances,
                Search {
                    query: query.into(),
                    limit: Some(1),
                    ..Search::default()
                },
            )
            .unwrap();
            assert_eq!(first["sessions"][0]["id"], "session-0");
            assert_eq!(first["next_after_id"], "session-0");
            let second = search(
                &instances,
                Search {
                    after_id: Some("session-0".into()),
                    ..Search::default()
                },
            )
            .unwrap();
            assert_eq!(second["sessions"].as_array().unwrap().len(), 1);
            assert_eq!(second["sessions"][0]["id"], "session-1");
        }
        let archived = search(
            &instances,
            Search {
                include_archived: true,
                exclude_session_id: Some("session-0".into()),
                ..Search::default()
            },
        )
        .unwrap();
        assert_eq!(archived["sessions"].as_array().unwrap().len(), 2);
        assert!(resolve(&instances, "session-3").is_err());
        assert!(resolve(&instances, "other-profile-session").is_err());
        assert!(
            parse::<SessionRead>(&json!({"session_id": "session-0", "profile": "other"})).is_err()
        );
        assert!(
            parse::<ActivityRead>(&json!({"session_id": "session-0", "path": "/tmp"})).is_err()
        );
    }

    #[test]
    fn recent_windows_normalize_and_bound_unicode() {
        for (input, lines, chars, expected, truncated) in [
            ("one\ntwo\nthree", 2, 100, "two\nthree", true),
            ("é界🙂abc", 10, 4, "🙂abc", true),
            ("\x1b[31mred\x1b[0m\x07\r", 10, 100, "red", false),
            ("\x1b[200~paste\x1b[201~", 10, 100, "paste", false),
            ("\x1bPsecret\x1b\\text", 10, 100, "text", false),
            (
                "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
                10,
                100,
                "link",
                false,
            ),
        ] {
            assert_eq!(
                recent_text(input, lines, chars),
                (expected.into(), truncated)
            );
        }
    }

    #[test]
    fn details_and_activity_preserve_unknown_states() {
        let mut instance = Instance::new("session", "/work");
        instance.view = View::Structured;
        let detail = details(&instance, None);
        for field in [
            "worktree",
            "sandbox",
            "last_activity_at_ms",
            "last_accessed_at",
        ] {
            assert!(detail[field].is_null(), "{field}");
        }
        assert_eq!(detail["status_freshness"], "stored_not_live");
        instance.agent_name = Some("codex".into());
        assert_eq!(details(&instance, None)["agent"], "codex");
        let switched = search(
            std::slice::from_ref(&instance),
            Search {
                agent: Some("codex".into()),
                ..Search::default()
            },
        )
        .unwrap();
        assert_eq!(switched["sessions"][0]["id"], instance.id);
        assert_eq!(
            activity(&instance, None, 100, 16_000)["state"],
            "unavailable"
        );
    }

    #[test]
    fn structured_activity_uses_only_selected_recent_events() {
        let temp = tempfile::tempdir().unwrap();
        let store = EventStore::open(&temp.path().join("events.db"), 1000).unwrap();
        let mut instance = Instance::new("selected", "/work");
        instance.view = View::Structured;
        store
            .record(
                "other-session",
                1,
                &Event::AgentMessageChunk {
                    text: "private".into(),
                },
            )
            .unwrap();
        assert_eq!(
            activity(&instance, Some(&store), 2, 100)["state"],
            "unavailable"
        );
        store
            .record(
                &instance.id,
                1,
                &Event::AgentMessageChunk {
                    text: "old\n\x1b[31mcurrent\x1b[0m\nBearer abcdefghijklmnopqrst".into(),
                },
            )
            .unwrap();
        let result = activity(&instance, Some(&store), 2, 100);
        assert_eq!(result["text"], "current\n<redacted-secret>");
        assert_eq!(result["truncated"], true);
        assert_eq!(result["source"], "structured_events");
        assert!(result["last_activity_at_ms"].is_number());
    }
}
