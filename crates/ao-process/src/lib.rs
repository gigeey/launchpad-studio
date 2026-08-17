pub mod supervisor;
pub mod registry;
pub mod mock;
#[cfg(unix)]
pub mod kill_tree;
pub mod default_supervisor;

pub use default_supervisor::shell_path;

#[cfg(test)]
mod tests {
    use crate::mock::{MockProcessSupervisor, MockScenario};
    use crate::registry::{RunRecord, RunRegistry, RunStatus};
    use crate::supervisor::{ProcessSupervisor, SpawnInput, TerminationReason};
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn make_spawn_input() -> SpawnInput {
        SpawnInput {
            run_id: None,
            backend_id: "test-backend".to_string(),
            scope_key: None,
            argv: vec!["echo".to_string(), "hello".to_string()],
            cwd: None,
            env: None,
            stdin_data: None,
            timeout_ms: None,
            no_output_timeout_ms: None,
            tools_in_flight: None,
            form_suspended: None,
        }
    }

    // ── MockProcessSupervisor tests ──

    #[tokio::test]
    async fn mock_spawn_collects_all_stdout_lines() {
        let mock = MockProcessSupervisor::new(vec![MockScenario {
            stdout_lines: vec![
                "line-1".to_string(),
                "line-2".to_string(),
                "line-3".to_string(),
            ],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 10,
        }]);

        let managed = mock.spawn(make_spawn_input()).await.unwrap();
        let mut stdout_rx = managed.stdout_rx;

        let mut collected = Vec::new();
        while let Some(line) = stdout_rx.recv().await {
            collected.push(line);
        }

        assert_eq!(collected, vec!["line-1", "line-2", "line-3"]);

        let exit = managed.wait_handle.await.unwrap();
        assert_eq!(exit.reason, TerminationReason::Natural);
        assert_eq!(exit.exit_code, Some(0));
    }

    #[tokio::test]
    async fn mock_spawn_cancel_stops_execution() {
        let mock = MockProcessSupervisor::new(vec![MockScenario {
            stdout_lines: (0..100).map(|i| format!("line-{}", i)).collect(),
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 50,
        }]);

        let managed = mock.spawn(make_spawn_input()).await.unwrap();

        // Wait a short time then cancel
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = managed.cancel_tx.send(TerminationReason::Cancelled);

        let exit = managed.wait_handle.await.unwrap();
        assert_eq!(exit.reason, TerminationReason::Cancelled);
        assert_eq!(exit.exit_code, None);
    }

    #[tokio::test]
    async fn mock_spawn_wait_handle_completes_naturally() {
        let mock = MockProcessSupervisor::new(vec![MockScenario {
            stdout_lines: vec!["hello".to_string()],
            stderr_lines: vec!["warn".to_string()],
            exit_code: 1,
            delay_per_line_ms: 5,
        }]);

        let managed = mock.spawn(make_spawn_input()).await.unwrap();
        let mut stderr_rx = managed.stderr_rx;
        let mut stdout_rx = managed.stdout_rx;

        // Drain channels
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();

        let exit = managed.wait_handle.await.unwrap();

        // Drain remaining messages after process exit
        while let Ok(line) = stdout_rx.try_recv() {
            stdout_lines.push(line);
        }
        while let Ok(line) = stderr_rx.try_recv() {
            stderr_lines.push(line);
        }

        assert_eq!(exit.reason, TerminationReason::Natural);
        assert_eq!(exit.exit_code, Some(1));
        assert!(exit.duration_ms < 1000); // Should complete quickly
    }

    // ── RunRegistry tests ──

    #[test]
    fn registry_register_and_list_active() {
        let registry = RunRegistry::new();

        registry.register(RunRecord {
            run_id: "run-1".to_string(),
            backend_id: "be-1".to_string(),
            pid: Some(1234),
            started_at: Utc::now(),
            scope_key: None,
            status: RunStatus::Running,
        });

        let active = registry.list_active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].run_id, "run-1");
    }

    #[test]
    fn registry_update_status_removes_from_active() {
        let registry = RunRegistry::new();

        registry.register(RunRecord {
            run_id: "run-1".to_string(),
            backend_id: "be-1".to_string(),
            pid: None,
            started_at: Utc::now(),
            scope_key: None,
            status: RunStatus::Running,
        });

        assert_eq!(registry.list_active().len(), 1);

        registry.update_status("run-1", RunStatus::Completed);
        assert_eq!(registry.list_active().len(), 0);

        let record = registry.get("run-1").unwrap();
        assert_eq!(record.status, RunStatus::Completed);
    }

    #[test]
    fn registry_remove_deletes_record() {
        let registry = RunRegistry::new();

        registry.register(RunRecord {
            run_id: "run-1".to_string(),
            backend_id: "be-1".to_string(),
            pid: None,
            started_at: Utc::now(),
            scope_key: None,
            status: RunStatus::Running,
        });

        registry.remove("run-1");
        assert!(registry.get("run-1").is_none());
        assert_eq!(registry.list_active().len(), 0);
    }

    #[tokio::test]
    async fn mock_list_active_tracks_running() {
        let mock = MockProcessSupervisor::new(vec![MockScenario {
            stdout_lines: vec!["slow".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 500,
        }]);

        let managed = mock.spawn(make_spawn_input()).await.unwrap();

        // While running, list_active should show it
        let active = mock.list_active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].run_id, managed.run_id);

        // Cancel and wait for completion
        let _ = managed.cancel_tx.send(TerminationReason::Cancelled);
        let _ = managed.wait_handle.await;

        // After completion, list_active should be empty
        let active = mock.list_active();
        assert_eq!(active.len(), 0);
    }

    // ── DefaultProcessSupervisor tests ──

    use crate::default_supervisor::DefaultProcessSupervisor;

    #[tokio::test]
    async fn default_spawn_echo_collects_stdout() {
        let supervisor = DefaultProcessSupervisor::new();

        let input = SpawnInput {
            run_id: None,
            backend_id: "test".to_string(),
            scope_key: None,
            argv: vec!["/bin/echo".to_string(), "hello".to_string()],
            cwd: None,
            env: None,
            stdin_data: None,
            timeout_ms: Some(5000),
            no_output_timeout_ms: None,
            tools_in_flight: None,
            form_suspended: None,
        };

        let managed = supervisor.spawn(input).await.unwrap();
        let mut stdout_rx = managed.stdout_rx;

        let mut collected = Vec::new();
        while let Some(line) = stdout_rx.recv().await {
            collected.push(line);
        }

        assert_eq!(collected, vec!["hello\n"]);

        let exit = managed.wait_handle.await.unwrap();
        assert_eq!(exit.reason, TerminationReason::Natural);
        assert_eq!(exit.exit_code, Some(0));
    }

    #[tokio::test]
    async fn default_spawn_cancel_stops_process() {
        let supervisor = DefaultProcessSupervisor::new();

        let input = SpawnInput {
            run_id: None,
            backend_id: "test".to_string(),
            scope_key: None,
            argv: vec!["/bin/sleep".to_string(), "10".to_string()],
            cwd: None,
            env: None,
            stdin_data: None,
            timeout_ms: None,
            no_output_timeout_ms: None,
            tools_in_flight: None,
            form_suspended: None,
        };

        let managed = supervisor.spawn(input).await.unwrap();
        let pid = managed.pid;

        // Cancel immediately
        let _ = managed.cancel_tx.send(TerminationReason::Cancelled);

        let exit = managed.wait_handle.await.unwrap();
        assert_eq!(exit.reason, TerminationReason::Cancelled);
        assert_eq!(exit.exit_code, None);

        // Verify process is dead
        if let Some(p) = pid {
            // Small delay to ensure OS cleanup
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            #[cfg(unix)]
            assert!(
                !crate::kill_tree::is_process_alive(p),
                "Process should be dead after cancel"
            );
        }
    }

    #[tokio::test]
    async fn default_spawn_timeout_kills_process() {
        let supervisor = DefaultProcessSupervisor::new();

        let input = SpawnInput {
            run_id: None,
            backend_id: "test".to_string(),
            scope_key: None,
            argv: vec!["/bin/sleep".to_string(), "60".to_string()],
            cwd: None,
            env: None,
            stdin_data: None,
            timeout_ms: Some(100),
            no_output_timeout_ms: None,
            tools_in_flight: None,
            form_suspended: None,
        };

        let managed = supervisor.spawn(input).await.unwrap();

        let exit = managed.wait_handle.await.unwrap();
        assert_eq!(exit.reason, TerminationReason::Timeout);
        assert!(exit.timed_out);
    }

    /// Reachability proof for the suspension-aware overall deadline: a run
    /// whose `form_suspended` counter stays pinned above zero for its entire
    /// lifetime must NOT be killed by a `timeout_ms` budget far shorter than
    /// its actual runtime — the deadline loop in `default_supervisor`'s
    /// Branch 2 must actually consult the counter on the live spawn path,
    /// not just in isolation. Paired with `default_spawn_timeout_kills_process`
    /// above (identical shape, no suspension wired) to prove the SAME budget
    /// behaves differently only because of the suspension signal.
    #[tokio::test]
    async fn default_spawn_timeout_paused_while_suspended() {
        let supervisor = DefaultProcessSupervisor::new();
        // Never drops back to 0 for the life of this run — simulates a form
        // that stays outstanding the whole time.
        let form_suspended = Arc::new(AtomicUsize::new(1));

        let input = SpawnInput {
            run_id: None,
            backend_id: "test".to_string(),
            scope_key: None,
            // Sleeps far longer than `timeout_ms` below; if the deadline
            // loop were still a plain one-shot sleep this would be killed
            // well before it exits naturally.
            argv: vec!["/bin/sleep".to_string(), "1".to_string()],
            cwd: None,
            env: None,
            stdin_data: None,
            timeout_ms: Some(100),
            no_output_timeout_ms: None,
            tools_in_flight: None,
            form_suspended: Some(form_suspended.clone()),
        };

        let managed = supervisor.spawn(input).await.unwrap();
        let exit = managed.wait_handle.await.unwrap();

        assert_eq!(
            exit.reason,
            TerminationReason::Natural,
            "a permanently-suspended run must run to completion, not time out"
        );
        assert!(!exit.timed_out);
    }

    /// Guards the constraint that the suspension signal is NOT an overload of
    /// `tools_in_flight`: a long-running tool call (e.g. `Bash`, a `Task`
    /// subagent) also pins `tools_in_flight > 0` for its duration, but that
    /// alone must NOT pause the overall wall-clock deadline — only a genuine
    /// `form_suspended` signal does. Without this guard, a bug that
    /// accidentally keyed Branch 2 off `tools_in_flight` instead of
    /// `form_suspended` would make every long tool call immortal too.
    #[tokio::test]
    async fn default_spawn_timeout_not_paused_by_tools_in_flight_alone() {
        let supervisor = DefaultProcessSupervisor::new();
        // Pinned above zero the whole run, as it would be for a long Bash
        // call — but this is NOT a form suspension.
        let tools_in_flight = Arc::new(AtomicUsize::new(1));

        let input = SpawnInput {
            run_id: None,
            backend_id: "test".to_string(),
            scope_key: None,
            argv: vec!["/bin/sleep".to_string(), "60".to_string()],
            cwd: None,
            env: None,
            stdin_data: None,
            timeout_ms: Some(100),
            no_output_timeout_ms: None,
            tools_in_flight: Some(tools_in_flight.clone()),
            form_suspended: None,
        };

        let managed = supervisor.spawn(input).await.unwrap();
        let exit = managed.wait_handle.await.unwrap();

        assert_eq!(
            exit.reason,
            TerminationReason::Timeout,
            "tools_in_flight alone must not pause the overall deadline"
        );
        assert!(exit.timed_out);
        // Sanity: the counter really was held above zero throughout, so this
        // isn't a false pass from a test that forgot to pin it.
        assert_eq!(tools_in_flight.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn default_list_active_tracks_running_and_completed() {
        let supervisor = DefaultProcessSupervisor::new();

        let input = SpawnInput {
            run_id: Some("run-echo".to_string()),
            backend_id: "test".to_string(),
            scope_key: None,
            argv: vec!["/bin/echo".to_string(), "quick".to_string()],
            cwd: None,
            env: None,
            stdin_data: None,
            timeout_ms: Some(5000),
            no_output_timeout_ms: None,
            tools_in_flight: None,
            form_suspended: None,
        };

        let managed = supervisor.spawn(input).await.unwrap();

        // Wait for completion
        let _ = managed.wait_handle.await;

        // After completion, list_active should be empty
        let active = supervisor.list_active();
        assert_eq!(active.len(), 0);

        // But get_record should still show the completed record
        let record = supervisor.get_record("run-echo");
        assert!(record.is_some());
        assert_eq!(record.unwrap().status, crate::registry::RunStatus::Completed);
    }
}
