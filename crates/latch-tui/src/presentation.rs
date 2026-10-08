//! Shared semantic state; terminal rendering stays in this crate.
pub use latch_ui::presentation::*;
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use latch_protocol::{CompletionState, Event, EventPayload, ToolCall, ToolResult};
    use serde_json::Value;
    use serde_json::json;
    use uuid::Uuid;

    fn event(payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            sequence: 1,
            timestamp: Utc::now(),
            parent_id: None,
            payload,
        }
    }

    fn request(id: &str, name: &str, arguments: Value) -> Event {
        event(EventPayload::ToolRequested {
            call: ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            },
        })
    }

    fn result(id: &str, name: &str, output: &str, failed: bool) -> Event {
        event(if failed {
            EventPayload::ToolFailed {
                result: ToolResult {
                    call_id: id.into(),
                    name: name.into(),
                    output: output.into(),
                    is_error: true,
                    artifact_id: None,
                    media: Vec::new(),
                },
            }
        } else {
            EventPayload::ToolCompleted {
                result: ToolResult {
                    call_id: id.into(),
                    name: name.into(),
                    output: output.into(),
                    is_error: false,
                    artifact_id: None,
                    media: Vec::new(),
                },
            }
        })
    }

    #[test]
    fn safe_shell_inspection_uses_exploration_semantics() {
        let model = PresentationModel::from_events(&[
            request("a", "shell", json!({"command":"ls -la src"})),
            request("b", "shell", json!({"command":"rg -n normalize src"})),
            result("a", "shell", "exit 0\nlib.rs", false),
            result("b", "shell", "exit 0\nsrc/lib.rs:1", false),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("List src"));
        assert!(rendered.contains("Search normalize"));
        assert!(!rendered.contains("Running ls"));
    }

    #[test]
    fn compound_shell_inspection_never_renders_raw_separators() {
        let model = PresentationModel::from_events(&[
            request(
                "a",
                "shell",
                json!({"command":"ls -la && cat Cargo.toml && ls src"}),
            ),
            request(
                "b",
                "shell",
                json!({"command":"git status && ls -R src && cat Cargo.toml"}),
            ),
            request(
                "c",
                "shell",
                json!({"command":"git log --oneline -5 | head"}),
            ),
            request("d", "shell", json!({"command":"echo hi && ls"})),
            result("a", "shell", "exit 0\nsrc", false),
            result("b", "shell", "exit 0\nsrc", false),
            result("c", "shell", "exit 0\ndeadbeef init", false),
            result("d", "shell", "exit 0\nhi\nsrc", false),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("List workspace · Read Cargo.toml · List src"));
        assert!(rendered.contains("Inspect git status · List src · Read Cargo.toml"));
        assert!(rendered.contains("Inspect git history · Read input"));
        // Unrecognized compound commands fall back to the generic Ran row.
        assert!(rendered.contains("Ran echo hi && ls"));
        for nonsense in ["List &&", "&&,", "cat, Cargo.toml", "ls, src", "List ,"] {
            assert!(
                !rendered.contains(nonsense),
                "raw command fragments leaked into the transcript: {nonsense}\n{rendered}"
            );
        }
    }

    #[test]
    fn failed_validation_and_multi_file_edit_render_semantically() {
        let model = PresentationModel::from_events(&[
            request(
                "v",
                "validate",
                json!({"requirement":"tests", "command":"cargo test"}),
            ),
            event(EventPayload::ValidationResult {
                command: "cargo test".into(),
                passed: false,
                detail: "exit code 1 (0.38s): average_preserves_fraction FAILED".into(),
            }),
            request(
                "a",
                "write",
                json!({"path":"tests/new.rs", "content":"one\ntwo"}),
            ),
            request(
                "b",
                "patch",
                json!({"path":"src/lib.rs", "old":"old", "new":"new"}),
            ),
            result("a", "write", "updated tests/new.rs @ abc", false),
            result("b", "patch", "updated src/lib.rs @ def", false),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("✗ Validation failed"));
        assert!(rendered.contains("cargo test · average_preserves_fraction FAILED · 0.38s"));
        assert!(rendered.contains("Edited 2 files"));
        assert!(rendered.contains("A tests/new.rs"));
        assert!(rendered.contains("M src/lib.rs"));
        assert!(!rendered.contains("abc"));
        assert!(!rendered.contains("call"));
    }

    #[test]
    fn semantic_history_snapshot() {
        let cells = vec![
            Cell::Exploration {
                operations: vec![
                    ExplorationOperation {
                        call_id: "a".into(),
                        label: "Read Cargo.toml".into(),
                        status: CellStatus::Passed,
                        diagnostic: String::new(),
                        raw: String::new(),
                    },
                    ExplorationOperation {
                        call_id: "b".into(),
                        label: "Read src/lib.rs".into(),
                        status: CellStatus::Passed,
                        diagnostic: String::new(),
                        raw: String::new(),
                    },
                ],
            },
            Cell::Patch {
                files: vec![PatchFile {
                    call_id: "p".into(),
                    path: "src/lib.rs".into(),
                    kind: 'M',
                    additions: 2,
                    deletions: 2,
                    status: CellStatus::Passed,
                    diagnostic: String::new(),
                    raw: String::new(),
                    preview: String::new(),
                }],
            },
            Cell::Validation {
                call_id: "v".into(),
                command: "cargo test".into(),
                requirement: "tests".into(),
                status: CellStatus::Passed,
                summary: "3 tests passed · 0.42s".into(),
                output: String::new(),
                raw: String::new(),
            },
        ];
        assert_eq!(
            super::super::render_cells_plain(&cells, false).trim_end(),
            include_str!("../tests/snapshots/v3_semantic.txt")
                .replace("\r\n", "\n")
                .trim_end()
        );
    }

    #[test]
    fn diff_cell_renders_delta_summary_and_bounded_body() {
        let model = PresentationModel::from_events(&[
            request("d", "git_diff", json!({})),
            result(
                "d",
                "git_diff",
                "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
                false,
            ),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("Workspace diff"));
        assert!(rendered.contains("+1"));
        assert!(rendered.contains("−1"));
        assert!(rendered.contains("@@ -1 +1 @@"));
    }

    #[test]
    fn narrow_failure_snapshot_content() {
        let cells = vec![
            Cell::User {
                text: "修复 failing test".into(),
                media: Vec::new(),
            },
            Cell::Command {
                call_id: "c".into(),
                command: "cargo test".into(),
                status: CellStatus::Failed,
                summary: "test failed".into(),
                output: "assertion failed".into(),
                raw: String::new(),
            },
        ];
        assert_eq!(
            super::super::render_cells_plain(&cells, false).trim_end(),
            include_str!("../tests/snapshots/v3_narrow.txt")
                .replace("\r\n", "\n")
                .trim_end()
        );
    }

    #[test]
    fn delivered_child_report_renders_one_compact_cell() {
        let mut model = PresentationModel::default();
        model.apply_event(&event(EventPayload::AgentNotificationDelivered {
            report: latch_protocol::AgentReport {
                report_id: Uuid::new_v4(),
                agent_id: Uuid::new_v4(),
                task_name: "audit-locks".into(),
                status: latch_protocol::AgentStatus::Completed,
                completion: CompletionState::InProgress,
                summary: "Found two unsynchronized locks.\nDetails omitted".into(),
                findings: vec![],
                touched_files: vec!["src/locks.rs".into()],
                evidence: vec![],
                unresolved_questions: vec![],
            },
        }));
        let [
            Cell::AgentReport {
                task_name,
                status,
                summary,
            },
        ] = model.cells()
        else {
            panic!("expected exactly one child-report cell");
        };
        assert_eq!(task_name, "audit-locks");
        assert_eq!(*status, latch_protocol::AgentStatus::Completed);
        assert_eq!(summary, "Found two unsynchronized locks.");
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(
            rendered.contains("✓ Child agent `audit-locks` completed"),
            "{rendered}"
        );
        assert!(
            rendered.contains("└ Found two unsynchronized locks."),
            "{rendered}"
        );
        assert!(
            !rendered.contains("Details omitted"),
            "only one bounded line crosses"
        );
    }

    #[test]
    fn agent_control_calls_stay_compact_in_root_history() {
        let model = PresentationModel::from_events(&[
            request(
                "s1",
                "spawn_agent",
                json!({"task_name":"audit-locks","message":"Review the lock ordering in the cache."}),
            ),
            result(
                "s1",
                "spawn_agent",
                "{\"agent_id\":\"00000000-0000-0000-0000-000000000001\",\"task_name\":\"audit-locks\",\"agent_type\":null,\"status\":\"running\"}",
                false,
            ),
            request("w1", "wait_agents", json!({"agent_ids":[]})),
            result(
                "w1",
                "wait_agents",
                "{\"agents\":[{\"agent_id\":\"00000000-0000-0000-0000-000000000001\",\"task_name\":\"audit-locks\",\"agent_type\":null,\"status\":\"completed\"}],\"reports_pending\":1}",
                false,
            ),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("• Spawned `audit-locks`"), "{rendered}");
        assert!(rendered.contains("child session running"), "{rendered}");
        assert!(rendered.contains("• Waited for agents"), "{rendered}");
        assert!(rendered.contains("0 running · 1 finished"), "{rendered}");
    }
}
