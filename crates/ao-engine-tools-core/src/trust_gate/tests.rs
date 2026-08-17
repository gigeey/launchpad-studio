use super::*;

fn request(
    artifact_type: ArtifactType,
    origin: CandidateOrigin,
    scope: CandidateScope,
    contradicts_existing: bool,
    overwrites_manual: bool,
) -> StagingRequest {
    StagingRequest {
        artifact_type,
        origin,
        scope,
        contradicts_existing,
        overwrites_manual,
    }
}

// ─── Table-driven: one row per tier ────────────────────────────
//
// Each row is a distinct combination of (artifact_type, origin, scope,
// contradicts_existing, overwrites_manual) paired with the tier the
// accepted boundary requires. Kept as one table (rather than one test per
// row) so the full policy surface is visible and auditable in one place.

struct Row {
    label: &'static str,
    artifact_type: ArtifactType,
    origin: CandidateOrigin,
    scope: CandidateScope,
    contradicts_existing: bool,
    overwrites_manual: bool,
    expected: StagingTier,
}

const ROWS: &[Row] = &[
    // --- Tier 1: AUTO-CONFIRM ---
    Row {
        label: "self-authored, new agent-scope memory, no contradiction -> auto-confirm",
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Agent,
        contradicts_existing: false,
        overwrites_manual: false,
        expected: StagingTier::AutoConfirm,
    },
    Row {
        label: "manual origin always auto-confirms, even a skill, even overwriting manual",
        artifact_type: ArtifactType::Skill,
        origin: CandidateOrigin::Manual,
        scope: CandidateScope::Global,
        contradicts_existing: true,
        overwrites_manual: true,
        expected: StagingTier::AutoConfirm,
    },
    // --- Tier 2: STAGE FOR REVIEW ---
    Row {
        label: "(2a) self-authored agent-scope skill, no contradiction -> still stages",
        artifact_type: ArtifactType::Skill,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Agent,
        contradicts_existing: false,
        overwrites_manual: false,
        expected: StagingTier::StageForReview,
    },
    Row {
        label: "(2b) self-authored memory contradicting an unverified entry -> stages",
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Agent,
        contradicts_existing: true,
        overwrites_manual: false,
        expected: StagingTier::StageForReview,
    },
    Row {
        label: "(2c/2d) self-authored memory written to project scope -> stages",
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Project,
        contradicts_existing: false,
        overwrites_manual: false,
        expected: StagingTier::StageForReview,
    },
    Row {
        label: "(2c/2d) self-authored memory written to global scope -> stages",
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Global,
        contradicts_existing: false,
        overwrites_manual: false,
        expected: StagingTier::StageForReview,
    },
    Row {
        label: "reflected memory, agent scope, no contradiction -> still stages (out-of-band)",
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::Reflected,
        scope: CandidateScope::Agent,
        contradicts_existing: false,
        overwrites_manual: false,
        expected: StagingTier::StageForReview,
    },
    Row {
        label: "reflected skill, agent scope, no contradiction -> stages",
        artifact_type: ArtifactType::Skill,
        origin: CandidateOrigin::Reflected,
        scope: CandidateScope::Agent,
        contradicts_existing: false,
        overwrites_manual: false,
        expected: StagingTier::StageForReview,
    },
    Row {
        label: "reserved workflow type, self-authored, agent scope -> stages (no rule defined yet)",
        artifact_type: ArtifactType::Workflow,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Agent,
        contradicts_existing: false,
        overwrites_manual: false,
        expected: StagingTier::StageForReview,
    },
    // --- Tier 3: NEVER-AUTO (hard block) ---
    Row {
        label: "(3) self-authored memory overwriting a Manual entry, agent scope -> hard block",
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Agent,
        contradicts_existing: true,
        overwrites_manual: true,
        expected: StagingTier::NeverAuto,
    },
    Row {
        label: "overwrites_manual wins even in project/global scope",
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Global,
        contradicts_existing: true,
        overwrites_manual: true,
        expected: StagingTier::NeverAuto,
    },
    Row {
        label: "overwrites_manual wins even for a reflected candidate",
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::Reflected,
        scope: CandidateScope::Agent,
        contradicts_existing: true,
        overwrites_manual: true,
        expected: StagingTier::NeverAuto,
    },
    Row {
        label: "overwrites_manual wins even for a skill candidate",
        artifact_type: ArtifactType::Skill,
        origin: CandidateOrigin::SelfAuthored,
        scope: CandidateScope::Agent,
        contradicts_existing: true,
        overwrites_manual: true,
        expected: StagingTier::NeverAuto,
    },
];

#[test]
fn table_driven_tier_matrix() {
    for row in ROWS {
        let decision = stage_candidate(request(
            row.artifact_type,
            row.origin,
            row.scope,
            row.contradicts_existing,
            row.overwrites_manual,
        ));
        assert_eq!(
            decision.tier, row.expected,
            "row failed: {} (got {:?})",
            row.label, decision.tier
        );
        assert!(!decision.reason.is_empty(), "row has empty reason: {}", row.label);
    }
}

// ─── Focused tests per the acceptance criteria's callouts ──────────────────

#[test]
fn manual_origin_always_auto_confirms_regardless_of_every_other_field() {
    let decision = stage_candidate(request(
        ArtifactType::Skill,
        CandidateOrigin::Manual,
        CandidateScope::Project,
        true,
        true,
    ));
    assert_eq!(decision.tier, StagingTier::AutoConfirm);
    assert!(decision.auto_enable());
    assert!(!decision.is_hard_blocked());
}

#[test]
fn agent_scope_new_non_contradicting_memory_auto_confirms() {
    let decision = stage_candidate(request(
        ArtifactType::Memory,
        CandidateOrigin::SelfAuthored,
        CandidateScope::Agent,
        false,
        false,
    ));
    assert_eq!(decision.tier, StagingTier::AutoConfirm);
    assert!(decision.auto_enable());
}

/// The Manual hard-block: overwriting a `Manual`/user-authored entry must
/// NEVER auto-apply, no matter the artifact type, origin, or scope — this is
/// the one rule with no exceptions except `CandidateOrigin::Manual` itself
/// (a human overwriting their own entry is always trusted).
#[test]
fn manual_hard_block_has_no_exceptions_for_non_manual_origin() {
    for artifact_type in [ArtifactType::Memory, ArtifactType::Skill, ArtifactType::Workflow] {
        for origin in [CandidateOrigin::SelfAuthored, CandidateOrigin::Reflected] {
            for scope in [CandidateScope::Agent, CandidateScope::Project, CandidateScope::Global] {
                let decision = stage_candidate(request(artifact_type, origin, scope, true, true));
                assert_eq!(
                    decision.tier,
                    StagingTier::NeverAuto,
                    "overwrites_manual must hard-block for artifact_type={artifact_type:?} \
                     origin={origin:?} scope={scope:?}"
                );
                assert!(!decision.auto_enable());
                assert!(decision.is_hard_blocked());
            }
        }
    }
}

/// Acceptance criterion: assert NO auto-enable path exists for a
/// model-invocable skill. Sweeps every origin/scope/contradiction
/// combination reachable for a non-Manual `Skill` candidate and requires
/// every single one to land outside `AutoConfirm`.
#[test]
fn no_auto_enable_path_exists_for_a_model_invocable_skill() {
    for origin in [CandidateOrigin::SelfAuthored, CandidateOrigin::Reflected] {
        for scope in [CandidateScope::Agent, CandidateScope::Project, CandidateScope::Global] {
            for contradicts_existing in [false, true] {
                for overwrites_manual in [false, true] {
                    // overwrites_manual implies contradicts_existing in every
                    // real caller, but the gate must be safe even if a
                    // caller ever passes this combination anyway.
                    let decision = stage_candidate(request(
                        ArtifactType::Skill,
                        origin,
                        scope,
                        contradicts_existing,
                        overwrites_manual,
                    ));
                    assert!(
                        !decision.auto_enable(),
                        "a Skill candidate auto-enabled for origin={origin:?} scope={scope:?} \
                         contradicts_existing={contradicts_existing} \
                         overwrites_manual={overwrites_manual}"
                    );
                }
            }
        }
    }
}

#[test]
fn reflected_origin_never_reaches_auto_confirm_regardless_of_scope_or_artifact_type() {
    for artifact_type in [ArtifactType::Memory, ArtifactType::Skill, ArtifactType::Workflow] {
        for scope in [CandidateScope::Agent, CandidateScope::Project, CandidateScope::Global] {
            let decision =
                stage_candidate(request(artifact_type, CandidateOrigin::Reflected, scope, false, false));
            assert!(
                !decision.auto_enable(),
                "Reflected origin auto-confirmed for artifact_type={artifact_type:?} scope={scope:?}"
            );
        }
    }
}

#[test]
fn gate_accepts_reserved_workflow_artifact_type() {
    // No producer emits `Workflow` yet, but the gate must already dispatch
    // on it without special-casing — that's the whole point of reserving
    // the variant now instead of retrofitting it later.
    let decision = stage_candidate(request(
        ArtifactType::Workflow,
        CandidateOrigin::SelfAuthored,
        CandidateScope::Agent,
        false,
        false,
    ));
    assert_eq!(decision.artifact_type, ArtifactType::Workflow);
    assert!(!decision.auto_enable());
}

#[test]
fn artifact_type_enum_has_exactly_the_three_documented_variants() {
    // Compile-time completeness check: if a variant is ever added or
    // renamed, this match forces every call site (including this test) to
    // be revisited rather than silently compiling with a missed arm.
    let assert_variant = |t: ArtifactType| match t {
        ArtifactType::Memory => "memory",
        ArtifactType::Skill => "skill",
        ArtifactType::Workflow => "workflow",
    };
    assert_eq!(assert_variant(ArtifactType::Memory), "memory");
    assert_eq!(assert_variant(ArtifactType::Skill), "skill");
    assert_eq!(assert_variant(ArtifactType::Workflow), "workflow");
}

#[test]
fn staging_tier_enum_has_exactly_the_three_documented_variants() {
    let assert_variant = |t: StagingTier| match t {
        StagingTier::AutoConfirm => "auto_confirm",
        StagingTier::StageForReview => "stage_for_review",
        StagingTier::NeverAuto => "never_auto",
    };
    assert_eq!(assert_variant(StagingTier::AutoConfirm), "auto_confirm");
    assert_eq!(assert_variant(StagingTier::StageForReview), "stage_for_review");
    assert_eq!(assert_variant(StagingTier::NeverAuto), "never_auto");
}

#[test]
fn artifact_type_serializes_snake_case() {
    let json = serde_json::to_string(&ArtifactType::Workflow).unwrap();
    assert_eq!(json, "\"workflow\"");
}

#[test]
fn staging_tier_serializes_snake_case() {
    assert_eq!(serde_json::to_string(&StagingTier::AutoConfirm).unwrap(), "\"auto_confirm\"");
    assert_eq!(
        serde_json::to_string(&StagingTier::StageForReview).unwrap(),
        "\"stage_for_review\""
    );
    assert_eq!(serde_json::to_string(&StagingTier::NeverAuto).unwrap(), "\"never_auto\"");
}

#[test]
fn decision_reason_is_non_empty_for_every_row_in_the_table() {
    for row in ROWS {
        let decision = stage_candidate(request(
            row.artifact_type,
            row.origin,
            row.scope,
            row.contradicts_existing,
            row.overwrites_manual,
        ));
        assert!(!decision.reason.is_empty(), "empty reason for row: {}", row.label);
    }
}
