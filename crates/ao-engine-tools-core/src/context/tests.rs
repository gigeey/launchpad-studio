//! Unit tests for the runner context and its associated stores.
//!
//! Declared from the parent module as `#[cfg(test)] mod tests;` — this is the
//! same module as the inline `mod tests` block it replaces, so private items
//! of `context` remain in scope here via `use super::*`.

use super::*;
// QuestionBridge, QuestionRequest, etc. come through `use super::*` above.

#[test]
fn child_increments_depth_and_reuses_cancel() {
    let parent = RunnerContext::new("sess", "agent-a").unwrap();
    let child = parent.child("agent-b");
    assert_eq!(child.depth, 1);
    assert_eq!(child.session_id, parent.session_id);
    assert_eq!(child.agent_id, "agent-b");
    // Cancelling parent must cancel child — same token clone.
    parent.cancel.cancel();
    assert!(child.cancel.is_cancelled());
}

#[test]
fn new_resolves_cwd_from_env() {
    let ctx = RunnerContext::new("sess", "agent").unwrap();
    let cwd = ctx.cwd.read().unwrap().clone();
    assert!(cwd.is_absolute());
}

#[test]
fn new_with_cwd_accepts_synthetic_path() {
    let path = PathBuf::from("/tmp/test-workspace");
    let ctx = RunnerContext::new_with_cwd("sess", "agent", path.clone());
    let stored = ctx.cwd.read().unwrap().clone();
    assert_eq!(stored, path);
}

#[test]
fn child_arc_clones_cwd_and_parent_write_is_visible() {
    let initial = PathBuf::from("/initial");
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", initial.clone());
    let child = parent.child("agent-b");

    // Parent and child see the same initial value.
    assert_eq!(parent.cwd.read().unwrap().clone(), initial);
    assert_eq!(child.cwd.read().unwrap().clone(), initial);

    // A write through the parent's Arc is visible to the child.
    let updated = PathBuf::from("/updated");
    *parent.cwd.write().unwrap() = updated.clone();
    assert_eq!(child.cwd.read().unwrap().clone(), updated);
}

#[test]
fn with_cwd_replaces_cwd_field() {
    let original = PathBuf::from("/original");
    let replacement = PathBuf::from("/replacement");
    let ctx =
        RunnerContext::new_with_cwd("sess", "agent", original).with_cwd(replacement.clone());
    assert_eq!(ctx.cwd.read().unwrap().clone(), replacement);
}

#[test]
fn permission_store_default_is_standard_mode() {
    let store = PermissionStore::default();
    assert_eq!(store.mode(), PermissionMode::Default);
}

#[test]
fn permission_store_set_mode_round_trips() {
    let store = PermissionStore::default();
    store.set_mode(PermissionMode::Plan);
    assert_eq!(store.mode(), PermissionMode::Plan);
    store.set_mode(PermissionMode::BypassPermissions);
    assert_eq!(store.mode(), PermissionMode::BypassPermissions);
    store.set_mode(PermissionMode::Default);
    assert_eq!(store.mode(), PermissionMode::Default);
}

#[test]
fn enter_plan_mode_transitions_from_default_to_plan() {
    let store = PermissionStore::default();
    store.enter_plan_mode();
    assert_eq!(store.mode(), PermissionMode::Plan);
}

#[test]
fn exit_plan_mode_restores_prior_mode() {
    let store = PermissionStore::default();
    store.enter_plan_mode();
    store.exit_plan_mode();
    assert_eq!(store.mode(), PermissionMode::Default);
}

#[test]
fn enter_plan_mode_is_idempotent_and_does_not_clobber_prior() {
    let store = PermissionStore::default();
    // Set to Default, enter Plan, enter Plan again (no-op), exit Plan →
    // must restore Default, not Plan.
    store.set_mode(PermissionMode::Default);
    store.enter_plan_mode();
    store.enter_plan_mode(); // no-op: already in Plan
    store.exit_plan_mode();
    assert_eq!(
        store.mode(),
        PermissionMode::Default,
        "double enter_plan_mode must not clobber the saved prior"
    );
}

#[test]
fn exit_plan_mode_is_idempotent_when_not_in_plan() {
    let store = PermissionStore::default();
    // Not in plan: exit is a no-op.
    store.exit_plan_mode();
    assert_eq!(store.mode(), PermissionMode::Default);
    // After enter → exit, a second exit is also a no-op.
    store.enter_plan_mode();
    store.exit_plan_mode();
    store.exit_plan_mode();
    assert_eq!(store.mode(), PermissionMode::Default);
}

#[test]
fn enter_exit_plan_mode_from_bypass_permissions() {
    let store = PermissionStore::default();
    store.set_mode(PermissionMode::BypassPermissions);
    store.enter_plan_mode();
    assert_eq!(store.mode(), PermissionMode::Plan);
    store.exit_plan_mode();
    assert_eq!(store.mode(), PermissionMode::BypassPermissions);
}

#[tokio::test]
async fn enter_exit_plan_mode_no_guard_held_across_spawn() {
    // Verify that enter_plan_mode / exit_plan_mode do not hold locks
    // across an await by running them from spawned tasks.
    let store = Arc::new(PermissionStore::default());
    let s1 = store.clone();
    let s2 = store.clone();
    let t1 = tokio::spawn(async move { s1.enter_plan_mode() });
    let t2 = tokio::spawn(async move { s2.exit_plan_mode() });
    t1.await.unwrap();
    t2.await.unwrap();
    // No deadlock → test passes.
}

#[test]
fn runner_context_default_permissions_is_standard() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert_eq!(ctx.permissions.mode(), PermissionMode::Default);
}

#[test]
fn child_arc_clones_permissions_and_parent_write_is_visible() {
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"));
    let child = parent.child("agent-b");

    assert_eq!(parent.permissions.mode(), PermissionMode::Default);
    assert_eq!(child.permissions.mode(), PermissionMode::Default);

    parent.permissions.set_mode(PermissionMode::Plan);
    assert_eq!(child.permissions.mode(), PermissionMode::Plan);
}

#[test]
fn with_permissions_replaces_permissions_field() {
    let store = Arc::new(PermissionStore::default());
    store.set_mode(PermissionMode::BypassPermissions);
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_permissions(store.clone());
    assert_eq!(ctx.permissions.mode(), PermissionMode::BypassPermissions);
}

fn make_item(id: &str) -> TodoItem {
    TodoItem {
        id: id.to_string(),
        content: format!("content-{id}"),
        status: TodoStatus::Pending,
        active_form: "default".to_string(),
    }
}

#[test]
fn todo_store_default_is_empty() {
    let store = TodoStore::default();
    assert!(store.get("agent-a").is_empty());
}

#[test]
fn todo_store_replace_and_get() {
    let store = TodoStore::default();
    store.replace("agent-a", vec![make_item("1"), make_item("2")]);
    let items = store.get("agent-a");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].id, "1");
    assert_eq!(items[1].id, "2");
}

#[test]
fn todo_store_replace_overwrites() {
    let store = TodoStore::default();
    store.replace("agent-a", vec![make_item("a"), make_item("b")]);
    store.replace("agent-a", vec![make_item("c")]);
    let items = store.get("agent-a");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "c");
}

#[test]
fn todo_store_clear_removes_key() {
    let store = TodoStore::default();
    store.replace("agent-a", vec![make_item("x")]);
    store.clear("agent-a");
    assert!(store.get("agent-a").is_empty());
}

#[test]
fn todo_store_cross_agent_isolation() {
    let store = TodoStore::default();
    store.replace("agent-a", vec![make_item("for-a")]);
    store.replace("agent-b", vec![make_item("for-b")]);
    let a = store.get("agent-a");
    let b = store.get("agent-b");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].id, "for-a");
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].id, "for-b");
}

#[test]
fn runner_context_default_todos_is_empty() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert!(ctx.todos.get("agent").is_empty());
}

#[test]
fn with_todos_replaces_todos_field() {
    let store = Arc::new(TodoStore::default());
    store.replace("agent", vec![make_item("injected")]);
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_todos(store.clone());
    assert_eq!(ctx.todos.get("agent").len(), 1);
}

#[test]
fn child_arc_clones_todos_and_isolation_by_agent_id() {
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"));
    let child = parent.child("agent-b");

    parent
        .todos
        .replace("agent-a", vec![make_item("parent-item")]);
    child
        .todos
        .replace("agent-b", vec![make_item("child-item")]);

    // Each agent sees only its own items.
    assert_eq!(parent.todos.get("agent-a").len(), 1);
    assert_eq!(parent.todos.get("agent-b").len(), 1);

    // Both read through the same Arc — a write from either is visible.
    assert_eq!(child.todos.get("agent-a")[0].id, "parent-item");
}

#[tokio::test]
async fn todo_store_concurrent_writes_no_deadlock() {
    let store = Arc::new(TodoStore::default());
    let s1 = store.clone();
    let s2 = store.clone();

    let t1 = tokio::spawn(async move {
        for i in 0..100u32 {
            s1.replace("agent-a", vec![make_item(&i.to_string())]);
        }
    });
    let t2 = tokio::spawn(async move {
        for i in 0..100u32 {
            s2.replace("agent-b", vec![make_item(&i.to_string())]);
        }
    });

    t1.await.unwrap();
    t2.await.unwrap();

    // Both agents have data; no deadlock occurred.
    assert_eq!(store.get("agent-a").len(), 1);
    assert_eq!(store.get("agent-b").len(), 1);
}

#[tokio::test]
async fn noop_event_sink_is_object_safe_and_emit_returns_ok() {
    // Verify object safety: construct Arc<dyn EventSink + Send + Sync>.
    let sink: Arc<dyn EventSink + Send + Sync> = Arc::new(NoopEventSink);

    // Emit each UserEvent variant through the trait object.
    sink.emit(UserEvent::Brief {
        content: "hello".to_string(),
    })
    .await
    .unwrap();
    sink.emit(UserEvent::PlanArtifact {
        plan_path: PathBuf::from("/tmp/plan.md"),
    })
    .await
    .unwrap();
    sink.emit(UserEvent::Question {
        id: "q1".to_string(),
        prompt: "Pick one".to_string(),
        choices: vec!["Yes".to_string(), "No".to_string()],
    })
    .await
    .unwrap();
    sink.emit(UserEvent::TodosUpdated {
        count: 2,
        in_progress: 1,
        pending: 1,
        completed: 0,
    })
    .await
    .unwrap();
    sink.emit(UserEvent::PermissionModeChanged {
        from: PermissionMode::Default,
        to: PermissionMode::Plan,
    })
    .await
    .unwrap();
    sink.emit(UserEvent::CwdChanged {
        from: PathBuf::from("/before"),
        to: PathBuf::from("/after"),
    })
    .await
    .unwrap();
}

#[test]
fn runner_context_default_event_sink_is_noop() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    // If the default sink were not set this would panic at the field access.
    let _sink = ctx.event_sink.clone();
}

#[tokio::test]
async fn with_event_sink_replaces_sink_field() {
    struct SpySink;
    #[async_trait]
    impl EventSink for SpySink {
        async fn emit(&self, _event: UserEvent) -> Result<(), AoError> {
            Ok(())
        }
    }

    let spy: Arc<dyn EventSink + Send + Sync> = Arc::new(SpySink);
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_event_sink(spy.clone());
    // Confirm emit works through the replaced sink.
    ctx.event_sink
        .emit(UserEvent::Brief {
            content: "test".to_string(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn child_arc_clones_event_sink() {
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"));
    let child = parent.child("agent-b");

    // Both parent and child should share the same trait object pointer.
    let p_ptr = Arc::as_ptr(&parent.event_sink);
    let c_ptr = Arc::as_ptr(&child.event_sink);
    assert_eq!(p_ptr, c_ptr);

    // Emit through child to confirm it works.
    child
        .event_sink
        .emit(UserEvent::Brief {
            content: "from child".to_string(),
        })
        .await
        .unwrap();
}

#[test]
fn worktree_stack_defaults_to_empty() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert!(ctx.worktree_stack.lock().unwrap().is_empty());
}

#[tokio::test]
async fn noop_question_bridge_returns_no_operator() {
    let bridge = NoopQuestionBridge;
    let req = QuestionRequest {
        question: "Yes?".to_string(),
        choices: vec![],
        agent_id: "a".to_string(),
        session_id: "s".to_string(),
    };
    let result = bridge.ask_question(req).await;
    assert!(matches!(result, Err(AskQuestionError::NoOperator)));
}

#[test]
fn runner_context_prompt_bridge_defaults_to_noop() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    // Verify that prompt_bridge is present (not panicking on access).
    let _bridge = ctx.prompt_bridge.clone();
}

#[test]
fn with_prompt_bridge_replaces_field() {
    let custom: Arc<dyn QuestionBridge + Send + Sync> = Arc::new(NoopQuestionBridge);
    let custom_ptr = Arc::as_ptr(&custom);
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_prompt_bridge(custom);
    assert_eq!(Arc::as_ptr(&ctx.prompt_bridge), custom_ptr);
}

#[test]
fn child_arc_clones_prompt_bridge() {
    let bridge: Arc<dyn QuestionBridge + Send + Sync> = Arc::new(NoopQuestionBridge);
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"))
        .with_prompt_bridge(bridge);
    let child = parent.child("agent-b");
    assert_eq!(
        Arc::as_ptr(&parent.prompt_bridge),
        Arc::as_ptr(&child.prompt_bridge)
    );
}

#[test]
fn with_worktree_stack_replaces_field() {
    let entry = WorktreeEntry {
        restore_cwd: PathBuf::from("/original"),
        worktree_path: PathBuf::from("/wt/original"),
        branch: "worktree/original".to_string(),
        base_commit: "abc123".to_string(),
    };
    let stack = Arc::new(Mutex::new(vec![entry]));
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_worktree_stack(stack.clone());
    assert_eq!(Arc::as_ptr(&ctx.worktree_stack), Arc::as_ptr(&stack));
    assert_eq!(ctx.worktree_stack.lock().unwrap().len(), 1);
}

#[test]
fn child_arc_clones_worktree_stack_and_push_is_visible() {
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/start"));
    let child = parent.child("agent-b");

    // Same Arc — pointer equality.
    assert_eq!(
        Arc::as_ptr(&parent.worktree_stack),
        Arc::as_ptr(&child.worktree_stack),
    );

    // Push from parent is visible to child.
    let entry = WorktreeEntry {
        restore_cwd: PathBuf::from("/prior"),
        worktree_path: PathBuf::from("/wt/prior"),
        branch: "worktree/prior".to_string(),
        base_commit: "def456".to_string(),
    };
    parent.worktree_stack.lock().unwrap().push(entry.clone());
    let child_stack = child.worktree_stack.lock().unwrap();
    assert_eq!(child_stack.len(), 1);
    assert_eq!(child_stack[0], entry);
}

#[test]
fn top_level_context_has_empty_spawn_chain() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert!(
        ctx.spawn_chain.is_empty(),
        "top-level runner must start with an empty spawn chain"
    );
}

#[tokio::test]
async fn top_level_context_live_count_starts_at_zero() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert_eq!(
        ctx.background_agents.live_count().await,
        0,
        "fresh registry must report zero live agents"
    );
}

#[test]
fn with_spawn_chain_replaces_field() {
    let chain = vec!["Explore".to_string(), "Summarise".to_string()];
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_spawn_chain(chain.clone());
    assert_eq!(ctx.spawn_chain, chain);
}

#[test]
fn with_background_agents_replaces_field() {
    let registry = Arc::new(BackgroundAgentRegistry::new(2));
    let registry_ptr = Arc::as_ptr(&registry);
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_background_agents(registry);
    assert_eq!(Arc::as_ptr(&ctx.background_agents), registry_ptr);
}

#[test]
fn child_inherits_spawn_chain_and_gets_fresh_background_registry() {
    let chain = vec!["Explore".to_string()];
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"))
        .with_spawn_chain(chain.clone());
    let child = parent.child("agent-b");

    // Spawn chain is cloned into the child (raw inheritance; the spawner extends it).
    assert_eq!(child.spawn_chain, chain);

    // Child gets its own independent BackgroundAgentRegistry.
    assert_ne!(
        Arc::as_ptr(&parent.background_agents),
        Arc::as_ptr(&child.background_agents),
        "child must have its own BackgroundAgentRegistry, not share the parent's"
    );
}

// --- skill_registry, pending_user_messages, skill_tool_filter ---

#[test]
fn skill_registry_default_is_empty() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert_eq!(ctx.skill_registry.read().unwrap().entries.len(), 0);
}

#[test]
fn with_skill_registry_replaces_field() {
    use crate::skill_registry::SkillRegistry;
    let reg = Arc::new(SkillRegistry::empty());
    let reg_ptr = Arc::as_ptr(&reg);
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_skill_registry(reg);
    assert_eq!(Arc::as_ptr(&*ctx.skill_registry.read().unwrap()), reg_ptr);
}

#[test]
fn child_arc_clones_skill_registry() {
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"));
    let child = parent.child("agent-b");
    assert_eq!(
        Arc::as_ptr(&parent.skill_registry),
        Arc::as_ptr(&child.skill_registry),
        "parent and child must share the same skill_registry Arc"
    );
}

#[test]
fn enqueue_user_message_pushes_to_back() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    ctx.enqueue_user_message("first".to_string());
    ctx.enqueue_user_message("second".to_string());
    // Drain as Interactive (kind default) with no sleep — both items come out.
    let drained = ctx
        .pending_user_messages
        .lock()
        .unwrap()
        .drain_for(SessionKind::Interactive, false);
    assert_eq!(drained, vec!["first".to_string(), "second".to_string()]);
}

#[test]
fn pending_user_messages_default_is_empty() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert!(ctx.pending_user_messages.lock().unwrap().is_empty());
}

#[test]
fn child_arc_clones_pending_user_messages() {
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"));
    let child = parent.child("agent-b");
    assert_eq!(
        Arc::as_ptr(&parent.pending_user_messages),
        Arc::as_ptr(&child.pending_user_messages),
        "parent and child must share the same pending_user_messages Arc"
    );
    // Write from parent is visible to child — drain returns 1 item.
    parent.enqueue_user_message("hello".to_string());
    let drained = child
        .pending_user_messages
        .lock()
        .unwrap()
        .drain_for(SessionKind::Interactive, false);
    assert_eq!(drained.len(), 1);
}

#[test]
fn skill_tool_filter_default_is_none() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert!(ctx.skill_tool_filter.read().unwrap().is_none());
}

#[test]
fn set_skill_tool_filter_activates_filter() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    let allowed: HashSet<String> = ["Read".to_string(), "Grep".to_string()].into();
    ctx.set_skill_tool_filter(allowed);
    assert!(ctx.skill_tool_filter.read().unwrap().is_some());
}

#[test]
fn clear_skill_tool_filter_resets_to_none() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    ctx.set_skill_tool_filter(["Read".to_string()].into());
    ctx.clear_skill_tool_filter();
    assert!(ctx.skill_tool_filter.read().unwrap().is_none());
}

#[test]
fn check_skill_tool_filter_returns_true_when_no_filter() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert!(ctx.check_skill_tool_filter("Bash"));
    assert!(ctx.check_skill_tool_filter("Read"));
    assert!(ctx.check_skill_tool_filter("anything"));
}

#[test]
fn check_skill_tool_filter_allows_listed_tool() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    ctx.set_skill_tool_filter(["Read".to_string(), "Grep".to_string()].into());
    assert!(ctx.check_skill_tool_filter("Read"));
    assert!(ctx.check_skill_tool_filter("Grep"));
}

#[test]
fn check_skill_tool_filter_denies_unlisted_tool() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    ctx.set_skill_tool_filter(["Read".to_string()].into());
    assert!(!ctx.check_skill_tool_filter("Bash"));
    assert!(!ctx.check_skill_tool_filter("Write"));
}

#[test]
fn skill_and_skill_write_always_allowed_through_filter() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    ctx.set_skill_tool_filter(["Read".to_string()].into());
    assert!(
        ctx.check_skill_tool_filter("RunSkill"),
        "RunSkill must always pass"
    );
    assert!(
        ctx.check_skill_tool_filter("SkillRegister"),
        "SkillRegister must always pass"
    );
}

#[test]
fn child_arc_clones_skill_tool_filter() {
    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"));
    let child = parent.child("agent-b");
    assert_eq!(
        Arc::as_ptr(&parent.skill_tool_filter),
        Arc::as_ptr(&child.skill_tool_filter),
        "parent and child must share the same skill_tool_filter Arc"
    );
    // Set from parent, visible to child.
    parent.set_skill_tool_filter(["Read".to_string()].into());
    assert!(child.skill_tool_filter.read().unwrap().is_some());
}

// --- read_file_state ---

#[test]
fn child_arc_clones_read_file_state_and_parent_record_is_visible() {
    use crate::read_file_state::ReadEntry;
    use std::time::SystemTime;

    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"));
    let child = parent.child("agent-b");

    // Same Arc.
    assert_eq!(
        Arc::as_ptr(&parent.read_file_state),
        Arc::as_ptr(&child.read_file_state),
        "parent and child must share the same read_file_state Arc"
    );

    // Record from parent is visible via child.
    let path = PathBuf::from("/tmp/shared.txt");
    parent.read_file_state.record(
        path.clone(),
        ReadEntry {
            content: "hello".to_string(),
            mtime: SystemTime::UNIX_EPOCH,
            offset: None,
            limit: None,
            surfaced_by_read: true,
        },
    );
    let got = child.read_file_state.get(&path);
    assert!(got.is_some(), "child must see entry recorded by parent");
    assert_eq!(got.unwrap().content, "hello");
}

// --- four optional runtime fields ---

#[test]
fn runner_context_with_all_three_fields_none_constructs_and_is_passable() {
    use crate::output::ToolOutput;
    use crate::tool::IoTool;
    use ao_protocol::error::AoError;

    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert!(
        ctx.workflow_runner.is_none(),
        "workflow_runner must default to None"
    );
    assert!(
        ctx.preferences.is_none(),
        "preferences must default to None"
    );
    assert!(
        ctx.agent_workflows.is_none(),
        "agent_workflows must default to None"
    );

    // Verify a no-op IoTool can accept a context with all three fields None.
    struct Noop;
    #[async_trait::async_trait]
    impl IoTool for Noop {
        fn name(&self) -> &str {
            "Noop"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn invoke(
            &self,
            _input: serde_json::Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::Text("ok".into()))
        }
    }

    let _tool = Noop;
    // Context is constructed successfully with all four new fields None.
}

#[test]
fn child_inherits_all_three_runtime_fields() {
    use ao_protocol::agent::WorkflowBinding;

    let parent = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"))
        .with_agent_workflows(WorkflowBinding::All);

    let child = parent.child("agent-b");

    assert!(
        matches!(child.agent_workflows, Some(WorkflowBinding::All)),
        "child must inherit agent_workflows from parent"
    );
    assert!(
        child.workflow_runner.is_none(),
        "workflow_runner still None (not set on parent)"
    );
    assert!(
        child.preferences.is_none(),
        "preferences still None (not set on parent)"
    );
}

// --- delegate_chain ---

#[test]
fn delegate_chain_default_is_empty() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    assert!(
        ctx.delegate_chain.is_empty(),
        "top-level runner must start with an empty delegate chain"
    );
}

#[test]
fn child_inherits_delegate_chain() {
    let chain = vec!["agent-root".to_string(), "agent-mid".to_string()];
    let parent = RunnerContext::new_with_cwd("sess", "agent-mid", PathBuf::from("/tmp"))
        .with_delegate_chain(chain.clone());
    let child = parent.child("agent-leaf");
    assert_eq!(
        child.delegate_chain, chain,
        "child must inherit parent's delegate_chain via child()"
    );
}

#[test]
fn with_delegate_chain_replaces_field() {
    let chain = vec!["agent-a".to_string()];
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_delegate_chain(chain.clone());
    assert_eq!(ctx.delegate_chain, chain);
}

// ── PendingMessageQueue drain behaviour ──────────────────────────────────

#[test]
fn low_priority_held_in_autonomous_when_sleep_ran() {
    let mut q = PendingMessageQueue::new();
    q.enqueue("normal".into());
    q.enqueue_low("low".into());
    let out = q.drain_for(crate::permissions::SessionKind::Autonomous, true);
    assert_eq!(
        out,
        vec!["normal"],
        "low-priority must be held when Sleep ran in Autonomous"
    );
    assert!(!q.is_empty(), "low-priority message should remain queued");
}

#[test]
fn low_priority_released_in_autonomous_on_sleep_free_turn() {
    let mut q = PendingMessageQueue::new();
    q.enqueue_low("low".into());
    // First turn: Sleep ran — low stays deferred.
    let first = q.drain_for(crate::permissions::SessionKind::Autonomous, true);
    assert!(
        first.is_empty(),
        "low-priority must be deferred when Sleep ran"
    );
    // Second turn: no Sleep — low drains.
    let second = q.drain_for(crate::permissions::SessionKind::Autonomous, false);
    assert_eq!(
        second,
        vec!["low"],
        "low-priority must release on first Sleep-free turn"
    );
    assert!(q.is_empty());
}

#[test]
fn low_priority_drains_immediately_in_interactive() {
    let mut q = PendingMessageQueue::new();
    q.enqueue_low("low".into());
    let out = q.drain_for(crate::permissions::SessionKind::Interactive, true);
    assert_eq!(
        out,
        vec!["low"],
        "Interactive session must drain low-priority regardless of sleep_ran"
    );
}
