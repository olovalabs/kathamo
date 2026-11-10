//! Reader-side compaction with explicit backpressure, never an idle debounce.
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use super::Incoming;
use crate::agent_bridge::{AgentEvent, COALESCE_TEXT_LIMIT};

const MAX_RECORDS: usize = 1_024;

#[derive(Default)]
struct State {
    records: VecDeque<Incoming>,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    space: Condvar,
}

#[derive(Clone)]
pub(super) struct Sender(Arc<Shared>);
pub(super) struct Receiver(Arc<Shared>);

pub(super) fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        space: Condvar::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl Sender {
    pub(super) fn send(&self, incoming: Incoming) -> Result<(), ()> {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(());
        }
        match (state.records.back_mut(), incoming) {
            (
                Some(Incoming::Delta(AgentEvent::Text(previous))),
                Incoming::Delta(AgentEvent::Text(next)),
            )
            | (
                Some(Incoming::Delta(AgentEvent::Thinking(previous))),
                Incoming::Delta(AgentEvent::Thinking(next)),
            ) if previous.len() + next.len() <= COALESCE_TEXT_LIMIT => {
                previous.push_str(&next);
                return Ok(());
            }
            (
                Some(Incoming::WorkerDelta {
                    id: previous_id,
                    role: previous_role,
                    text: previous,
                    timestamp: previous_timestamp,
                }),
                Incoming::WorkerDelta {
                    id,
                    role,
                    text,
                    timestamp,
                },
            ) if *previous_id == id
                && *previous_role == role
                && *previous_timestamp == timestamp
                && previous.len() + text.len() <= COALESCE_TEXT_LIMIT =>
            {
                previous.push_str(&text);
                return Ok(());
            }
            (_, incoming) => {
                // A run of cumulative snapshots may replace the same call's old
                // snapshot. Never cross text, worker, status or lifecycle records.
                if let Incoming::Record {
                    event:
                        Some(AgentEvent::ToolUpdate {
                            id: Some(id), name, ..
                        }),
                    ..
                } = &incoming
                {
                    let previous = state
                        .records
                        .iter()
                        .enumerate()
                        .rev()
                        .take_while(|(_, record)| {
                            matches!(
                                record,
                                Incoming::Record {
                                    event: Some(AgentEvent::ToolUpdate { .. }),
                                    ..
                                }
                            )
                        })
                        .find_map(|(index, record)| match record {
                            Incoming::Record {
                                event:
                                    Some(AgentEvent::ToolUpdate {
                                        id: Some(previous_id),
                                        name: previous_name,
                                        ..
                                    }),
                                ..
                            } if previous_id == id && previous_name == name => Some(index),
                            _ => None,
                        });
                    if let Some(index) = previous {
                        state.records.remove(index);
                    }
                }
                while state.records.len() >= MAX_RECORDS && !state.closed {
                    state = self
                        .0
                        .space
                        .wait(state)
                        .unwrap_or_else(|error| error.into_inner());
                }
                if state.closed {
                    return Err(());
                }
                state.records.push_back(incoming);
            }
        }
        Ok(())
    }
}

impl Receiver {
    pub(super) fn try_recv(&self) -> Result<Incoming, ()> {
        // Rendering must never wait on a producer that is compacting a record.
        let Ok(mut state) = self.0.state.try_lock() else {
            crate::agent_bridge::signal_wake();
            return Err(());
        };
        let incoming = state.records.pop_front().ok_or(())?;
        self.0.space.notify_one();
        Ok(incoming)
    }

    pub(super) fn close(&self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        state.records.clear();
        self.0.space.notify_all();
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(id: &str, output: &str) -> Incoming {
        Incoming::Record {
            record: json!({"type":"tool_execution_update","toolCallId":id,"partialResult":{"content":[{"type":"text","text":output}]}}),
            event: Some(AgentEvent::ToolUpdate {
                id: Some(id.into()),
                name: "bash".into(),
                output: output.into(),
            }),
            approval_error: false,
            received: std::time::Instant::now(),
        }
    }

    #[test]
    fn queued_deltas_preserve_all_text_and_structural_order() {
        let (sender, receiver) = channel();
        for _ in 0..100 {
            sender
                .send(Incoming::Delta(AgentEvent::Text("α".repeat(100))))
                .unwrap();
        }
        sender.send(Incoming::Boundary).unwrap();
        sender
            .send(Incoming::Delta(AgentEvent::Text("after block".into())))
            .unwrap();
        sender
            .send(Incoming::Delta(AgentEvent::Thinking("reason".into())))
            .unwrap();
        sender
            .send(Incoming::Delta(AgentEvent::Thinking("ing".into())))
            .unwrap();
        sender
            .send(Incoming::Error("status boundary".into()))
            .unwrap();
        sender
            .send(Incoming::Delta(AgentEvent::Text("last".into())))
            .unwrap();
        sender.send(Incoming::Closed).unwrap();
        let mut text = String::new();
        let mut observed = Vec::new();
        while let Ok(record) = receiver.try_recv() {
            match record {
                Incoming::Delta(AgentEvent::Text(delta)) => {
                    assert!(delta.len() <= COALESCE_TEXT_LIMIT);
                    text.push_str(&delta);
                    observed.push("text");
                }
                Incoming::Boundary => observed.push("block"),
                Incoming::Delta(AgentEvent::Thinking(delta)) => {
                    assert_eq!(delta, "reasoning");
                    observed.push("reasoning");
                }
                Incoming::Error(error) => {
                    assert_eq!(error, "status boundary");
                    observed.push("error");
                }
                Incoming::Closed => observed.push("closed"),
                _ => panic!("unexpected record"),
            }
        }
        assert_eq!(text, format!("{}after blocklast", "α".repeat(10_000)));
        assert_eq!(
            observed,
            [
                "text",
                "text",
                "block",
                "text",
                "reasoning",
                "error",
                "text",
                "closed"
            ]
        );
    }

    #[test]
    fn latest_tool_snapshots_keep_their_newest_position_and_lifecycle_barriers() {
        let (sender, receiver) = channel();
        sender.send(snapshot("a", "old")).unwrap();
        sender.send(snapshot("b", "independent")).unwrap();
        sender
            .send(snapshot("a", "complete cumulative output"))
            .unwrap();
        sender.send(Incoming::Boundary).unwrap();
        sender.send(snapshot("a", "next lifecycle")).unwrap();
        sender.send(Incoming::Closed).unwrap();
        let mut observed = Vec::new();
        while let Ok(record) = receiver.try_recv() {
            match record {
                Incoming::Record {
                    event: Some(AgentEvent::ToolUpdate { id, output, .. }),
                    record,
                    ..
                } => {
                    assert_eq!(record["partialResult"]["content"][0]["text"], output);
                    observed.push((id.unwrap(), output));
                }
                Incoming::Boundary => observed.push(("boundary".into(), String::new())),
                Incoming::Closed => observed.push(("closed".into(), String::new())),
                _ => panic!("unexpected record"),
            }
        }
        assert_eq!(
            observed,
            [
                ("b".into(), "independent".into()),
                ("a".into(), "complete cumulative output".into()),
                ("boundary".into(), String::new()),
                ("a".into(), "next lifecycle".into()),
                ("closed".into(), String::new()),
            ]
        );
    }

    #[test]
    fn worker_changes_are_never_merged_across_identity_or_timestamp() {
        let (sender, receiver) = channel();
        for (id, timestamp, text) in [
            ("a", 1, "first"),
            ("a", 1, " second"),
            ("b", 1, "other"),
            ("a", 2, "next"),
        ] {
            sender
                .send(Incoming::WorkerDelta {
                    id: id.into(),
                    timestamp: Some(timestamp),
                    role: crate::ChatRole::Assistant,
                    text: text.into(),
                })
                .unwrap();
        }
        let mut observed = Vec::new();
        while let Ok(record) = receiver.try_recv() {
            let Incoming::WorkerDelta {
                id,
                timestamp,
                text,
                ..
            } = record
            else {
                panic!("expected worker delta");
            };
            observed.push((id, timestamp.unwrap(), text));
        }
        assert_eq!(
            observed,
            [
                ("a".into(), 1, "first second".into()),
                ("b".into(), 1, "other".into()),
                ("a".into(), 2, "next".into()),
            ]
        );
    }

    #[test]
    fn full_queue_backpressures_without_losing_lifecycle_records() {
        let (sender, receiver) = channel();
        for _ in 0..MAX_RECORDS {
            sender.send(Incoming::Boundary).unwrap();
        }
        let producer = std::thread::spawn(move || sender.send(Incoming::Closed));
        loop {
            if let Ok(incoming) = receiver.try_recv() {
                assert!(matches!(incoming, Incoming::Boundary));
                break;
            }
            std::thread::yield_now();
        }
        producer.join().unwrap().unwrap();
        for _ in 1..MAX_RECORDS {
            assert!(matches!(receiver.try_recv(), Ok(Incoming::Boundary)));
        }
        assert!(matches!(receiver.try_recv(), Ok(Incoming::Closed)));
    }

    #[test]
    fn tool_start_result_status_and_completion_reach_the_consumer_in_order() {
        let (sender, receiver) = channel();
        let result = "complete result\n".repeat(2_000);
        let fixtures = [
            (
                json!({"type":"tool_execution_start"}),
                AgentEvent::ToolStart {
                    id: Some("call".into()),
                    name: "bash".into(),
                    input: "{}".into(),
                },
            ),
            (
                json!({"type":"tool_execution_end","result":{"text":result}}),
                AgentEvent::ToolEnd {
                    id: Some("call".into()),
                    name: "bash".into(),
                    output: result.clone(),
                    failed: false,
                },
            ),
        ];
        for (record, event) in fixtures {
            let starts_call = matches!(&event, AgentEvent::ToolStart { .. });
            sender
                .send(Incoming::Record {
                    record,
                    event: Some(event),
                    approval_error: false,
                    received: std::time::Instant::now(),
                })
                .unwrap();
            if starts_call {
                sender.send(snapshot("call", "old partial")).unwrap();
                sender.send(snapshot("call", "latest partial")).unwrap();
            }
        }
        sender
            .send(Incoming::Delta(AgentEvent::Status("finished tools".into())))
            .unwrap();
        sender.send(Incoming::Delta(AgentEvent::Done)).unwrap();
        let mut events = Vec::new();
        while let Ok(incoming) = receiver.try_recv() {
            match incoming {
                Incoming::Record {
                    event: Some(event), ..
                }
                | Incoming::Delta(event) => events.push(event),
                _ => panic!("unexpected record"),
            }
        }
        assert!(matches!(
            events.as_slice(),
            [
                AgentEvent::ToolStart { .. },
                AgentEvent::ToolUpdate { .. },
                AgentEvent::ToolEnd { .. },
                AgentEvent::Status(_),
                AgentEvent::Done,
            ]
        ));
        let AgentEvent::ToolUpdate { output, .. } = &events[1] else {
            unreachable!()
        };
        assert_eq!(output, "latest partial");
        let AgentEvent::ToolEnd { output, failed, .. } = &events[2] else {
            unreachable!()
        };
        assert_eq!(output, &result);
        assert!(!failed);
    }
}
