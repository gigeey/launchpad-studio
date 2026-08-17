use super::*;
use ao_persistence::paths::DataRoot;

// --- (b) conservative cold start -----------------------------------------

#[test]
fn zero_history_is_a_tight_cold_start_budget() {
    let controller = PromotionBudgetController::new();
    assert_eq!(controller.acceptance_rate(), None);
    assert_eq!(
        controller.budget(),
        MIN_BUDGET,
        "no history yet must resolve to the tight end of the range, never the loose end"
    );
}

#[test]
fn cold_start_budget_is_strictly_tighter_than_a_high_acceptance_budget() {
    let cold = PromotionBudgetController::new();
    let mut warmed = PromotionBudgetController::new();
    for _ in 0..WINDOW_SIZE {
        warmed.record(ReviewDecision::Accepted);
    }
    assert!(
        cold.budget() < warmed.budget(),
        "cold start ({}) must start tighter than an earned high-acceptance budget ({})",
        cold.budget(),
        warmed.budget()
    );
}

// --- (a) keeps raise the budget, forgets lower it ------------------------

#[test]
fn a_run_of_keeps_raises_the_computed_budget() {
    let mut controller = PromotionBudgetController::new();
    let cold_start_budget = controller.budget();

    for _ in 0..WINDOW_SIZE {
        controller.record(ReviewDecision::Accepted);
    }

    assert_eq!(controller.acceptance_rate(), Some(1.0));
    assert!(
        controller.budget() > cold_start_budget,
        "a full window of keeps ({}) must raise the budget above cold start ({cold_start_budget})",
        controller.budget()
    );
    assert_eq!(controller.budget(), MAX_BUDGET, "100% acceptance must reach the loose end");
}

#[test]
fn a_run_of_forgets_lowers_the_computed_budget_from_an_earned_baseline() {
    let mut controller = PromotionBudgetController::new();
    // Earn a high budget first, exactly like the "raises" test.
    for _ in 0..WINDOW_SIZE {
        controller.record(ReviewDecision::Accepted);
    }
    let earned_budget = controller.budget();

    // Now a run of forgets rolls those accepts out of the count window.
    for _ in 0..WINDOW_SIZE {
        controller.record(ReviewDecision::Rejected);
    }

    assert_eq!(controller.acceptance_rate(), Some(0.0));
    assert!(
        controller.budget() < earned_budget,
        "a full window of forgets ({}) must lower the budget below the earned baseline ({earned_budget})",
        controller.budget()
    );
}

#[test]
fn budget_moves_monotonically_with_acceptance_rate() {
    // Sweep the mix from all-rejected to all-accepted within one window and
    // confirm the budget never decreases as the accepted share grows.
    let mut last_budget = 0;
    for accepted_count in 0..=WINDOW_SIZE {
        let mut controller = PromotionBudgetController::new();
        for i in 0..WINDOW_SIZE {
            let decision = if i < accepted_count {
                ReviewDecision::Accepted
            } else {
                ReviewDecision::Rejected
            };
            controller.record(decision);
        }
        let budget = controller.budget();
        assert!(
            budget >= last_budget,
            "budget must not decrease as acceptance rate rises (accepted_count={accepted_count})"
        );
        last_budget = budget;
    }
}

// --- (c) exploration floor: budget never reaches 0 -----------------------

#[test]
fn budget_never_reaches_zero_even_after_all_rejections() {
    let mut controller = PromotionBudgetController::new();
    for _ in 0..(WINDOW_SIZE * 5) {
        controller.record(ReviewDecision::Rejected);
    }
    assert_eq!(controller.acceptance_rate(), Some(0.0));
    assert!(controller.budget() > 0, "the exploration floor must hold: budget was 0");
    assert_eq!(controller.budget(), MIN_BUDGET);
}

#[test]
fn gate_still_grants_a_trickle_after_a_long_rejection_streak() {
    let mut controller = PromotionBudgetController::new();
    for _ in 0..(WINDOW_SIZE * 3) {
        controller.record(ReviewDecision::Rejected);
    }
    let mut gate = PromotionBudgetGate::new(controller);
    assert!(
        gate.try_reserve(),
        "even an all-reject history must still grant at least one promotion — the exploration \
         floor is what lets the controller ever recover new signal"
    );
}

// --- (d) driven only by keep/forget events, never judge confidence -------

#[test]
fn review_decision_has_no_confidence_or_rationale_field() {
    // Structural guarantee: ReviewDecision is a plain two-variant enum with
    // no payload at all, so there is no field a judge's confidence score or
    // rationale string could ever occupy. This test exists to make that
    // guarantee explicit and to break loudly if the enum ever grows one.
    let accepted = ReviewDecision::Accepted;
    let rejected = ReviewDecision::Rejected;
    assert_ne!(accepted, rejected);
}

#[test]
fn identical_accept_reject_sequences_yield_identical_budgets_regardless_of_provenance() {
    // One sequence built directly; another derived by round-tripping through
    // the OutcomeRecord instrumentation shape, carrying unrelated per-turn
    // "noise" (different candidate ids, timestamps, and detail text) that
    // has nothing to do with any judge confidence value. Only the
    // accepted/rejected pattern should matter to the resulting budget.
    let sequence = [
        ReviewDecision::Accepted,
        ReviewDecision::Accepted,
        ReviewDecision::Rejected,
        ReviewDecision::Accepted,
    ];

    let direct = PromotionBudgetController::from_history(sequence.iter().copied());

    let mut history = Vec::new();
    for (i, decision) in sequence.iter().enumerate() {
        history.push(OutcomeRecord {
            turn_id: format!("candidate-{i}"),
            session_id: "agent-x".to_string(),
            artifacts_used: vec![ArtifactRef::memory(format!("candidate-{i}"))],
            signal: OutcomeSignal::Explicit {
                positive: matches!(decision, ReviewDecision::Accepted),
                detail: Some(format!(
                    "{REVIEW_DECISION_DETAIL_PREFIX}{}",
                    if matches!(decision, ReviewDecision::Accepted) { "accepted" } else { "rejected" }
                )),
            },
            timestamp: Utc::now(),
        });
    }
    let via_outcome_records =
        PromotionBudgetController::from_history(decisions_from_outcome_history(&history));

    assert_eq!(direct.budget(), via_outcome_records.budget());
    assert_eq!(direct.acceptance_rate(), via_outcome_records.acceptance_rate());
}

#[test]
fn decisions_from_outcome_history_ignores_unrelated_implicit_and_negative_records() {
    // Ordinary per-turn OutcomeRecords (not human staging-gate decisions at
    // all) must never be mistaken for acceptance-rate signal, no matter
    // what their signal looks like.
    let history = vec![
        OutcomeRecord {
            turn_id: "turn-1".to_string(),
            session_id: "agent-x".to_string(),
            artifacts_used: vec![],
            signal: OutcomeSignal::Implicit,
            timestamp: Utc::now(),
        },
        OutcomeRecord {
            turn_id: "turn-2".to_string(),
            session_id: "agent-x".to_string(),
            artifacts_used: vec![],
            signal: OutcomeSignal::Negative { detail: Some("that's wrong".to_string()) },
            timestamp: Utc::now(),
        },
        // An Explicit record that is NOT tagged as a promotion-review
        // decision (e.g. a future thumbs-up feature) must also be ignored.
        OutcomeRecord {
            turn_id: "turn-3".to_string(),
            session_id: "agent-x".to_string(),
            artifacts_used: vec![],
            signal: OutcomeSignal::Explicit { positive: true, detail: Some("thumbs_up".to_string()) },
            timestamp: Utc::now(),
        },
    ];

    let decisions = decisions_from_outcome_history(&history);
    assert!(decisions.is_empty());
}

// --- instrumentation: record_review_decision / decisions_from_outcome_history ---

#[tokio::test]
async fn record_review_decision_round_trips_through_the_outcome_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store = OutcomeStore::new(DataRoot::new(tmp.path()));

    record_review_decision(&store, "agent-1", "cand-1", ReviewDecision::Accepted)
        .await
        .unwrap();
    record_review_decision(&store, "agent-1", "cand-2", ReviewDecision::Rejected)
        .await
        .unwrap();

    let history = store.read_all("agent-1").await.unwrap();
    assert_eq!(history.len(), 2);

    let decisions = decisions_from_outcome_history(&history);
    assert_eq!(decisions, vec![ReviewDecision::Accepted, ReviewDecision::Rejected]);
}

#[tokio::test]
async fn controller_built_from_a_stores_recorded_keeps_and_forgets_reflects_the_true_rate() {
    let tmp = tempfile::tempdir().unwrap();
    let store = OutcomeStore::new(DataRoot::new(tmp.path()));

    for i in 0..3 {
        record_review_decision(&store, "agent-1", &format!("kept-{i}"), ReviewDecision::Accepted)
            .await
            .unwrap();
    }
    record_review_decision(&store, "agent-1", "forgotten-1", ReviewDecision::Rejected)
        .await
        .unwrap();

    let history = store.read_all("agent-1").await.unwrap();
    let controller = PromotionBudgetController::from_history(decisions_from_outcome_history(&history));

    assert_eq!(controller.len(), 4);
    assert_eq!(controller.acceptance_rate(), Some(0.75));
}

// --- PromotionBudgetGate cap enforcement ---------------------------------

#[test]
fn gate_blocks_once_the_cycles_budget_is_exhausted() {
    // Force a small, known budget by staying at cold start (MIN_BUDGET).
    let mut gate = PromotionBudgetGate::new(PromotionBudgetController::new());
    assert_eq!(gate.budget(), MIN_BUDGET);

    let mut granted = 0;
    for _ in 0..(MIN_BUDGET * 3) {
        if gate.try_reserve() {
            granted += 1;
        }
    }
    assert_eq!(
        granted, MIN_BUDGET,
        "the gate must never grant more than the current cycle's budget"
    );
}

#[test]
fn gate_rolls_over_into_a_fresh_cycle_after_window_size_attempts() {
    let mut gate = PromotionBudgetGate::new(PromotionBudgetController::new());
    let budget = gate.budget();

    // Exhaust the first cycle's budget, then burn through the remaining
    // attempts in that cycle (all denied), until WINDOW_SIZE attempts have
    // been made in total — the next attempt must start a fresh cycle.
    let mut total_attempts = 0;
    let mut total_granted = 0;
    while total_attempts < WINDOW_SIZE {
        if gate.try_reserve() {
            total_granted += 1;
        }
        total_attempts += 1;
    }
    assert_eq!(total_granted, budget);

    // The next attempt rolls into a new cycle and is granted again.
    assert!(gate.try_reserve(), "a fresh cycle must grant at least one promotion again");
}

#[test]
fn set_controller_refreshes_the_budget_without_resetting_cycle_usage() {
    let mut gate = PromotionBudgetGate::new(PromotionBudgetController::new());
    assert!(gate.try_reserve(), "cold start grants exactly MIN_BUDGET, so the first attempt succeeds");
    assert!(!gate.try_reserve(), "cold start's single slot is already used");

    // Refresh with a warmed, high-acceptance controller mid-cycle.
    let mut warmed = PromotionBudgetController::new();
    for _ in 0..WINDOW_SIZE {
        warmed.record(ReviewDecision::Accepted);
    }
    gate.set_controller(warmed);

    // Usage carries over (1 already used this cycle), but the higher budget
    // now allows more attempts before the cycle is exhausted again.
    assert!(gate.try_reserve(), "a raised budget must free up headroom within the same cycle");
}
