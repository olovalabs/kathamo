//! Transient extension dialogs; never part of the persisted agent transcript.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent_writer::AgentWriter;

struct Pending {
    method: String,
    deadline: Option<Instant>,
}

struct State {
    accepting: bool,
    prompt_id: Option<String>,
    pending: HashMap<String, Pending>,
    ready: Vec<Value>,
}

impl State {
    fn invalidate(&mut self, writer: &AgentWriter) {
        self.accepting = false;
        self.prompt_id = None;
        for id in self.pending.keys() {
            deny(writer, id);
        }
        self.pending.clear();
        self.ready.clear();
    }
}

#[derive(Clone)]
pub(super) struct UiRequests(Arc<Mutex<State>>);

impl Default for UiRequests {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(State {
            accepting: true,
            prompt_id: None,
            pending: HashMap::new(),
            ready: Vec::new(),
        })))
    }
}

fn deny(writer: &AgentWriter, id: &str) {
    // `timedOut:true` gives an ask dialog its recommended answers upstream.
    // A host-side expiry is a denial, never an implicit selection or approval.
    let _ = writer.send_json(json!({"type":"extension_ui_response","id":id,"cancelled":true}));
}

impl UiRequests {
    pub(super) fn receive(&self, record: Value, received: Instant, writer: &AgentWriter) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let method = record["method"].as_str().unwrap_or("");
        if method == "cancel" {
            if let Some(target) = record["targetId"].as_str() {
                state.pending.remove(target);
                state
                    .ready
                    .retain(|request| request["id"].as_str() != Some(target));
            }
        } else if matches!(method, "select" | "confirm" | "input" | "editor" | "ask") {
            let Some(id) = record["id"].as_str() else {
                return;
            };
            if !state.accepting {
                deny(writer, id);
                return;
            }
            let deadline = record["timeout"]
                .as_f64()
                .filter(|timeout| timeout.is_finite() && *timeout >= 0.0)
                .and_then(|timeout| Duration::try_from_secs_f64(timeout / 1_000.0).ok())
                .and_then(|timeout| received.checked_add(timeout));
            state.pending.insert(
                id.to_owned(),
                Pending {
                    method: method.to_owned(),
                    deadline,
                },
            );
        }
        state.ready.push(record);
    }

    pub(super) fn begin_prompt(&self, id: &str) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.accepting = true;
        state.prompt_id = Some(id.to_owned());
    }

    pub(super) fn finish_prompt(&self, id: &str, writer: &AgentWriter) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.prompt_id.as_deref() == Some(id) {
            state.invalidate(writer);
        }
    }

    pub(super) fn invalidate(&self, writer: &AgentWriter) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(writer);
    }

    pub(super) fn is_pending(&self, id: &str) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.pending.get(id).is_some_and(|request| {
            request
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline)
        })
    }

    pub(super) fn take(&self, writer: &AgentWriter) -> Vec<Value> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        state.pending.retain(|id, request| {
            if request.deadline.is_some_and(|deadline| now >= deadline) {
                deny(writer, id);
                false
            } else {
                true
            }
        });
        let mut ready = std::mem::take(&mut state.ready);
        ready.retain(|request| {
            !matches!(
                request["method"].as_str(),
                Some("select" | "confirm" | "input" | "editor" | "ask")
            ) || request["id"]
                .as_str()
                .is_some_and(|id| state.pending.contains_key(id))
        });
        ready
    }

    pub(super) fn respond(&self, response: Value, writer: &AgentWriter) -> Result<(), String> {
        if response["type"] != "extension_ui_response" {
            return Err("Expected an extension_ui_response record".into());
        }
        let id = response["id"]
            .as_str()
            .ok_or("Extension dialog response needs a string id")?
            .to_owned();
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let request = state
            .pending
            .get(&id)
            .ok_or("The extension dialog is no longer pending")?;
        if request
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            state.pending.remove(&id);
            deny(writer, &id);
            return Err("The extension dialog has timed out".into());
        }
        let valid = response["cancelled"] == true
            || match request.method.as_str() {
                "confirm" => response["confirmed"].is_boolean(),
                "select" | "input" | "editor" => response["value"].is_string(),
                "ask" => response["answers"].is_array(),
                _ => false,
            };
        if !valid {
            return Err("The response does not match this extension dialog".into());
        }
        writer.send_json(response)?;
        state.pending.remove(&id);
        state
            .ready
            .retain(|request| request["id"].as_str() != Some(id.as_str()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn sent(writer: &AgentWriter, sink: &Sink) -> Vec<Value> {
        writer.shutdown();
        String::from_utf8(sink.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn replies_preserve_dialog_ids_and_payloads_and_reject_duplicates() {
        let sink = Sink::default();
        let writer = AgentWriter::spawn(sink.clone(), |_| {});
        let requests = UiRequests::default();
        let input = json!({"type":"extension_ui_request","id":"secret-dialog","method":"input","title":"Token","secret":true});
        requests.receive(input.clone(), Instant::now(), &writer);
        assert_eq!(requests.take(&writer), [input]);
        assert!(requests.is_pending("secret-dialog"));
        assert!(
            requests
                .respond(
                    json!({"type":"extension_ui_response","id":"secret-dialog","confirmed":true}),
                    &writer
                )
                .is_err()
        );
        let reply =
            json!({"type":"extension_ui_response","id":"secret-dialog","value":"private token"});
        requests.respond(reply.clone(), &writer).unwrap();
        assert!(!requests.is_pending("secret-dialog"));
        assert!(requests.respond(reply.clone(), &writer).is_err());
        assert_eq!(sent(&writer, &sink), [reply]);
    }

    #[test]
    fn cancelled_and_expired_dialogs_cannot_be_approved() {
        let sink = Sink::default();
        let writer = AgentWriter::spawn(sink.clone(), |_| {});
        let requests = UiRequests::default();
        requests.receive(json!({"type":"extension_ui_request","id":"cancelled","method":"confirm","title":"Run?","message":"Exec"}), Instant::now(), &writer);
        requests.receive(json!({"type":"extension_ui_request","id":"cancel-notice","method":"cancel","targetId":"cancelled"}), Instant::now(), &writer);
        assert!(!requests.is_pending("cancelled"));
        assert!(
            requests
                .respond(
                    json!({"type":"extension_ui_response","id":"cancelled","confirmed":true}),
                    &writer
                )
                .is_err()
        );
        requests.receive(json!({"type":"extension_ui_request","id":"expired","method":"ask","questions":[],"timeout":0}), Instant::now(), &writer);
        assert!(!requests.is_pending("expired"));
        assert!(
            requests
                .respond(
                    json!({"type":"extension_ui_response","id":"expired","answers":[]}),
                    &writer
                )
                .is_err()
        );
        let ready = requests.take(&writer);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0]["method"], "cancel");
        assert_eq!(
            sent(&writer, &sink),
            [json!({"type":"extension_ui_response","id":"expired","cancelled":true})]
        );
    }

    #[test]
    fn terminal_invalidation_denies_pending_and_late_dialogs_until_next_prompt() {
        let sink = Sink::default();
        let writer = AgentWriter::spawn(sink.clone(), |_| {});
        let requests = UiRequests::default();
        for id in ["select", "editor", "confirm"] {
            requests.receive(
                json!({"type":"extension_ui_request","id":id,"method":id}),
                Instant::now(),
                &writer,
            );
        }
        requests.invalidate(&writer);
        requests.receive(
            json!({"type":"extension_ui_request","id":"late","method":"confirm"}),
            Instant::now(),
            &writer,
        );
        assert!(requests.take(&writer).is_empty());
        assert!(!requests.is_pending("late"));
        requests.begin_prompt("next-prompt");
        requests.receive(
            json!({"type":"extension_ui_request","id":"new","method":"confirm"}),
            Instant::now(),
            &writer,
        );
        requests
            .respond(
                json!({"type":"extension_ui_response","id":"new","confirmed":false}),
                &writer,
            )
            .unwrap();
        let frames = sent(&writer, &sink);
        assert_eq!(frames.len(), 5);
        for id in ["select", "editor", "confirm", "late"] {
            assert!(
                frames.contains(&json!({"type":"extension_ui_response","id":id,"cancelled":true}))
            );
        }
        assert_eq!(
            frames.last().unwrap(),
            &json!({"type":"extension_ui_response","id":"new","confirmed":false})
        );
    }

    #[test]
    fn an_old_title_result_does_not_cancel_the_current_prompt_dialog() {
        let sink = Sink::default();
        let writer = AgentWriter::spawn(sink.clone(), |_| {});
        let requests = UiRequests::default();
        requests.begin_prompt("current-prompt");
        requests.receive(json!({"type":"extension_ui_request","id":"approval","method":"select","options":["Approve","Deny"]}), Instant::now(), &writer);
        requests.finish_prompt("old-title", &writer);
        assert!(requests.is_pending("approval"));
        assert_eq!(requests.take(&writer).len(), 1);
        requests.finish_prompt("current-prompt", &writer);
        assert!(!requests.is_pending("approval"));
        assert!(
            requests
                .respond(
                    json!({"type":"extension_ui_response","id":"approval","value":"Approve"}),
                    &writer
                )
                .is_err()
        );
        assert_eq!(
            sent(&writer, &sink),
            [json!({"type":"extension_ui_response","id":"approval","cancelled":true})]
        );
    }
}
