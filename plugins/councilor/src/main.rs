use std::collections::VecDeque;
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

const BOOTSTRAP: &str = include_str!("../bootstrap-v1.md");

fn error(message: impl ToString) -> Value {
    json!({"code": -32603, "message": message.to_string()})
}

struct Worker<R, W> {
    input: R,
    output: W,
    pending: VecDeque<Value>,
    sequence: u64,
}

impl<R: BufRead, W: Write> Worker<R, W> {
    fn read(&mut self) -> Result<Option<Value>, Value> {
        let mut line = String::new();
        if self.input.read_line(&mut line).map_err(error)? == 0 {
            return Ok(None);
        }
        serde_json::from_str(&line).map(Some).map_err(error)
    }

    fn write(&mut self, value: &Value) -> Result<(), Value> {
        serde_json::to_writer(&mut self.output, value).map_err(error)?;
        writeln!(self.output).map_err(error)?;
        self.output.flush().map_err(error)
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.sequence += 1;
        let id = json!(format!("councilor-{}", self.sequence));
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let message = self.read()?.ok_or_else(|| error("host disconnected"))?;
            if message.get("method").is_some() {
                if message.get("id").is_some() {
                    self.pending.push_back(message);
                }
            } else if message["id"] == id {
                return match message.get("error") {
                    Some(err) => Err(err.clone()),
                    None => message
                        .get("result")
                        .cloned()
                        .ok_or_else(|| error("missing RPC result")),
                };
            }
        }
    }

    fn setting(&mut self, key: &str) -> Result<Option<String>, Value> {
        let response = self.call("config.get", json!({"key": key}))?;
        Ok(response["value"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned))
    }

    fn open(&mut self, params: &Value) -> Result<Value, Value> {
        let profile = params["profile"]
            .as_str()
            .ok_or_else(|| error("missing active profile"))?;
        let key = format!("chat:{profile}");
        let saved = self.call("plugin.storage.get", json!({"key": key}))?["value"].clone();
        if let Some(session_id) = saved.as_str() {
            match self.call("sessions.details", json!({"session_id": session_id})) {
                Ok(_) => return Ok(json!({"session_id": session_id})),
                Err(err) if err["data"]["kind"] == "session_not_found" => {}
                Err(err) => return Err(err),
            }
        } else if !saved.is_null() {
            return Err(error("saved Councilor session ID is invalid"));
        }
        let agent = self
            .setting("agent_id")?
            .or_else(|| params["agent_id"].as_str().map(str::to_owned))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| error("Configure an installed ACP-compatible agent in AoE settings."))?;
        let model = self.setting("model_id")?;
        let capabilities = self.call("acp.capabilities.get", json!({}))?;
        if !capabilities["agents"]
            .as_array()
            .is_some_and(|agents| agents.iter().any(|a| a["id"] == agent))
        {
            return Err(error(
                "Configure an installed ACP-compatible agent in AoE settings.",
            ));
        }
        let created = self.call(
            "sessions.create",
            json!({
                "agent_id": agent,
                "model_id": model,
                "sandbox": params["sandbox"].as_bool().unwrap_or(false),
                "title": "Councilor",
                "initial_turn": {"text": BOOTSTRAP},
                "idempotency_key": format!("councilor:{profile}"),
            }),
        )?;
        let session_id = created["session_id"]
            .as_str()
            .ok_or_else(|| error("host returned no session ID"))?;
        let saved = self.call(
            "plugin.storage.cas",
            json!({
                "key": key, "expected": saved, "value": session_id,
            }),
        )?;
        let session_id = saved["current"]
            .as_str()
            .ok_or_else(|| error("Councilor session changed; reopen the popup."))?;
        Ok(json!({"session_id": session_id}))
    }

    fn run(&mut self) -> Result<(), Value> {
        loop {
            let message = match self.pending.pop_front() {
                Some(message) => message,
                None => match self.read()? {
                    Some(message) => message,
                    None => return Ok(()),
                },
            };
            let Some(id) = message.get("id") else {
                continue;
            };
            let result = match message["method"].as_str() {
                Some("plugin.chat.open") => self.open(&message["params"]),
                _ => Err(json!({"code": -32601, "message": "unknown method"})),
            };
            let response = match result {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
            };
            self.write(&response)?;
        }
    }
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut worker = Worker {
        input: stdin.lock(),
        output: stdout.lock(),
        pending: VecDeque::new(),
        sequence: 0,
    };
    if let Err(err) = worker.run() {
        eprintln!("Councilor worker: {}", err["message"]);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumes_saved_session_and_preserves_transient_failures() {
        for response in [
            json!({"result": {"id": "saved"}}),
            json!({"error": {"code": -32603, "message": "storage unavailable"}}),
            json!({"error": {"code": -32005, "data": {"kind": "session_trashed"}}}),
        ] {
            let mut reply = response;
            reply["id"] = json!("councilor-2");
            let input = format!(
                "{}\n{}\n",
                json!({"id": "councilor-1", "result": {"value": "saved"}}),
                reply
            );
            let mut worker = Worker {
                input: io::Cursor::new(input),
                output: Vec::new(),
                pending: VecDeque::new(),
                sequence: 0,
            };
            let result = worker.open(&json!({"profile": "default"}));
            assert_eq!(result.is_err(), reply.get("error").is_some());
            let output = String::from_utf8(worker.output).unwrap();
            assert!(!output.contains("sessions.create"));
        }
    }

    #[test]
    fn queues_concurrent_opens_while_awaiting_host_reply() {
        let input = format!(
            "{}\n{}\n",
            json!({"id": 2, "method": "plugin.chat.open", "params": {}}),
            json!({"id": "councilor-1", "result": {"value": null}})
        );
        let mut worker = Worker {
            input: io::Cursor::new(input),
            output: Vec::new(),
            pending: VecDeque::new(),
            sequence: 0,
        };
        worker
            .call("plugin.storage.get", json!({"key": "chat:default"}))
            .unwrap();
        assert_eq!(worker.pending.pop_front().unwrap()["id"], 2);
    }

    #[test]
    fn creates_once_after_first_open_or_confirmed_deletion() {
        for previous in [Value::Null, json!("deleted")] {
            let mut replies = vec![json!({"result": {"value": previous}})];
            if !previous.is_null() {
                replies.push(json!({"error": {
                    "code": -32005, "data": {"kind": "session_not_found"},
                }}));
            }
            replies.extend([
                json!({"result": {"value": null}}),
                json!({"result": {"value": null}}),
                json!({"result": {"agents": [{"id": "claude"}]}}),
                json!({"result": {"session_id": "created", "created": true}}),
                json!({"result": {"swapped": true, "current": "created"}}),
            ]);
            let input: String = replies
                .into_iter()
                .enumerate()
                .map(|(index, mut reply)| {
                    reply["id"] = json!(format!("councilor-{}", index + 1));
                    format!("{reply}\n")
                })
                .collect();
            let mut worker = Worker {
                input: io::Cursor::new(input),
                output: Vec::new(),
                pending: VecDeque::new(),
                sequence: 0,
            };
            let result = worker
                .open(&json!({"profile": "default", "agent_id": "claude", "sandbox": true}))
                .unwrap();
            assert_eq!(result["session_id"], "created");
            let output = String::from_utf8(worker.output).unwrap();
            let calls: Vec<Value> = output
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let creates: Vec<_> = calls
                .iter()
                .filter(|call| call["method"] == "sessions.create")
                .collect();
            assert_eq!(creates.len(), 1);
            let params = &creates[0]["params"];
            assert_eq!(params["agent_id"], "claude");
            assert_eq!(params["sandbox"], true);
            assert_eq!(params["idempotency_key"], "councilor:default");
            assert_eq!(params["initial_turn"]["text"], BOOTSTRAP);
            assert!(params.get("project_path").is_none());
            assert!(params.get("mode_id").is_none());
            assert_eq!(calls.last().unwrap()["params"]["expected"], previous);
        }
    }
}
