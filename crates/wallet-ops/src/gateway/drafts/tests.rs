use super::*;

#[test]
fn private_execution_requires_native_approval_and_keeps_uncertain_outcomes_terminal() {
    use crate::gateway::GatewayPrivateDraftResult;
    let execution = GatewayDraftExecution::private("operation".into());
    let observer = execution.clone();
    assert!(!execution.start());
    let mut projection = execution.snapshot().private.unwrap();
    projection.execution_id = "old-operation".into();
    execution.update_private(
        GatewayDraftStatus::Done,
        String::new(),
        String::new(),
        false,
        projection,
    );
    assert_eq!(observer.snapshot().status, GatewayDraftStatus::Attention);
    assert!(execution.approve_review());
    assert!(execution.start());
    assert!(!observer.start());
    execution.bind_generation(7);
    let mut projection = execution.snapshot().private.unwrap();
    projection.result = Some(GatewayPrivateDraftResult::TimedOut);
    execution.update_private(
        GatewayDraftStatus::Failed,
        "Timed out".into(),
        String::new(),
        true,
        projection.clone(),
    );
    projection.result = None;
    observer.update_private(
        GatewayDraftStatus::InProgress,
        String::new(),
        String::new(),
        false,
        projection,
    );
    let terminal = observer.snapshot();
    assert_eq!(terminal.status, GatewayDraftStatus::Failed);
    assert_eq!(
        terminal.private.unwrap().result,
        Some(GatewayPrivateDraftResult::TimedOut)
    );
    assert!(!terminal.can_retry);
    assert!(!observer.approve_review());
}

#[test]
fn draft_payload_preserves_public_wire_inputs_and_separates_private_kinds() {
    let public = serde_json::json!({
        "account":"public", "chain_id":1, "kind":"send", "asset":"native",
        "amount":"1", "recipient":"recipient.eth", "address_book_entry":null,
        "fee":{"mode":"normal"}, "mimic_railway":false, "max":false
    });
    for kind in ["send", "shield"] {
        let mut wire = public.clone();
        wire["kind"] = kind.into();
        let parsed: GatewayDraftPayload = serde_json::from_value(wire.clone()).unwrap();
        // An input without an approval choice approves the exact amount.
        assert_eq!(
            parsed.public().map(|input| input.approval),
            Some(GatewayDraftApproval::Exact)
        );
        wire["approval"] = "unlimited".into();
        let parsed: GatewayDraftPayload = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            parsed.public().map(|input| input.approval),
            Some(GatewayDraftApproval::Unlimited)
        );
        assert_eq!(serde_json::to_value(parsed).unwrap(), wire);
    }
    let mut private = serde_json::json!({
        "wallet":"private", "chain_id":1, "kind":"private_send", "asset":"token",
        "amount":"1", "max":false, "recipient":"recipient", "address_book_entry":null,
        "fee_mode":"deduct", "unwrap":false, "native_top_up":false,
        "fee_token":"fee-token", "broadcaster":{"mode":"random"},
        "allow_out_of_range":false, "favorites_only":false
    });
    for kind in ["private_send", "unshield"] {
        private["kind"] = kind.into();
        let parsed: GatewayDraftPayload = serde_json::from_value(private.clone()).unwrap();
        assert!(matches!(parsed, GatewayDraftPayload::Private(_)));
        assert_eq!(serde_json::to_value(parsed).unwrap(), private);
        assert!(serde_json::from_value::<GatewayDraftInput>(private.clone()).is_err());
    }
    let mut self_broadcast = private.clone();
    for field in [
        "fee_token",
        "broadcaster",
        "allow_out_of_range",
        "favorites_only",
    ] {
        self_broadcast.as_object_mut().unwrap().remove(field);
    }
    self_broadcast["delivery"] = serde_json::json!({
        "mode":"self_broadcast", "signer":"explicit-signer",
        "funding":{"mode":"sponsorship", "incentive":{"mode":"custom", "percent":"7"}},
        "fee":{"mode":"custom", "max_fee_gwei":"2.125", "priority_fee_gwei":"0.1"}
    });
    let parsed: GatewayDraftPayload = serde_json::from_value(self_broadcast.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), self_broadcast);
    for field in [
        "fee_token",
        "broadcaster",
        "allow_out_of_range",
        "favorites_only",
    ] {
        let mut mixed = self_broadcast.clone();
        mixed[field] = private[field].clone();
        assert!(serde_json::from_value::<GatewayDraftPayload>(mixed).is_err());
    }
    for pointer in ["", "/delivery", "/delivery/funding", "/delivery/fee"] {
        let mut override_input = self_broadcast.clone();
        override_input.pointer_mut(pointer).unwrap()["rpc_url"] = "http://caller".into();
        assert!(serde_json::from_value::<GatewayDraftPayload>(override_input).is_err());
    }
    let mut mixed_funding = self_broadcast.clone();
    mixed_funding["delivery"]["funding"]["mode"] = "public_balance".into();
    assert!(serde_json::from_value::<GatewayDraftPayload>(mixed_funding).is_err());
    self_broadcast["delivery"]["funding"] = serde_json::json!({"mode":"public_balance"});
    self_broadcast["delivery"]["fee"] = serde_json::json!({"mode":"auto"});
    self_broadcast["delivery"]["signer"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<GatewayDraftPayload>(self_broadcast).is_ok());
    private["kind"] = "send".into();
    assert!(serde_json::from_value::<GatewayDraftPayload>(private).is_err());
}

#[test]
fn canceled_reviews_and_duplicate_approvals_cannot_start_another_transaction() {
    let canceled = GatewayDraftExecution::default();
    let late_approval = canceled.clone();
    canceled.cancel_review();
    assert!(!late_approval.approve_review());
    assert!(!late_approval.start());
    assert!(canceled.snapshot().can_retry);

    let execution = GatewayDraftExecution::default();
    let duplicate = execution.clone();
    assert!(!execution.start());
    assert!(execution.approve_review());
    execution.cancel_review(); // Closing the successfully approved dialog is not rejection.
    assert!(execution.require_prepared_review());
    assert!(!execution.start()); // Preparation must satisfy the approved terms before handoff.
    assert!(execution.approve_review());
    assert!(execution.start());
    assert!(!execution.require_prepared_review());
    assert!(!duplicate.start());
    execution.finish(false); // The task can finish before queued handoff events are delivered.
    assert!(!execution.snapshot().can_retry);

    let hardware = GatewayDraftExecution::private("hardware-password-approved".into());
    assert!(hardware.approve_review());
    hardware.cancel_review(); // The first password dialog closes to open device approval.
    hardware.cancel_authorization();
    assert!(!hardware.approve_review());
    assert!(!hardware.start());
    assert!(hardware.snapshot().can_retry);
    assert_eq!(
        hardware.snapshot().private.unwrap().result,
        Some(super::super::GatewayPrivateDraftResult::Cancelled)
    );
    let public = GatewayDraftExecution::default();
    assert!(public.approve_review());
    public.cancel_review();
    public.cancel_authorization();
    assert!(!public.approve_review());
    assert!(!public.start());
    assert!(public.snapshot().can_retry);
    assert_eq!(public.snapshot().status, GatewayDraftStatus::Failed);

    let running = GatewayDraftExecution::private("hardware-complete".into());
    assert!(running.approve_review());
    assert!(running.start());
    running.cancel_authorization();
    assert_eq!(running.snapshot().status, GatewayDraftStatus::InProgress);
    duplicate.event(&PublicActionSessionEvent::AttemptHandoff {
        step: PublicActionProgressStep::Send,
    });
    assert_eq!(execution.snapshot().status, GatewayDraftStatus::Failed);
    assert!(!execution.snapshot().can_retry);
}

#[test]
fn retiring_private_execution_disables_controls_and_retains_terminal_results() {
    use super::super::{GatewayPrivateDraftProgress, GatewayPrivateDraftResult};
    for submitted in [false, true] {
        let execution = GatewayDraftExecution::private("owned".into());
        assert!(execution.approve_review());
        assert!(execution.start());
        let progress = GatewayPrivateDraftProgress {
            execution_id: "owned".into(),
            result: submitted.then_some(GatewayPrivateDraftResult::Submitted),
            transaction_hash: submitted.then(|| "synthetic-hash".into()),
            stop: !submitted,
            favorite: submitted,
            ..Default::default()
        };
        let status = if submitted {
            GatewayDraftStatus::Done
        } else {
            GatewayDraftStatus::InProgress
        };
        execution.update_private(
            status,
            String::new(),
            String::new(),
            false,
            progress.clone(),
        );
        execution.retire_private_owner();
        execution.update_private(status, String::new(), String::new(), false, progress);
        let snapshot = execution.snapshot();
        let private = snapshot.private.unwrap();
        assert_eq!(
            private.result,
            Some(if submitted {
                GatewayPrivateDraftResult::Submitted
            } else {
                GatewayPrivateDraftResult::Stopped
            })
        );
        assert_eq!(
            private.transaction_hash.as_deref(),
            submitted.then_some("synthetic-hash")
        );
        assert!(!private.stop && !private.stop_waiting && !private.ban && !private.favorite);
        assert!(!snapshot.can_retry);
    }
}

#[test]
fn private_revalidation_failure_releases_approval_without_resetting_started_work() {
    let review = GatewayDraftExecution::private("review".into());
    assert!(review.approve_review());
    review.reject_private_review("Private funds changed".into());
    assert!(!review.start());
    assert!(review.snapshot().can_retry);
    assert_eq!(review.snapshot().message, "Private funds changed");
    review.cancel_review();
    assert_eq!(review.snapshot().message, "Private funds changed");
    assert!(review.snapshot().can_retry);
    assert!(!review.approve_review());

    let running = GatewayDraftExecution::private("running".into());
    assert!(running.approve_review());
    assert!(running.start());
    running.reject_private_review("A late estimate failed".into());
    running.cancel_review();
    assert_eq!(running.snapshot().status, GatewayDraftStatus::InProgress);
    assert!(!running.snapshot().can_retry);
}

#[test]
fn recurring_shield_attention_survives_observer_changes_and_terminal_state_is_stable() {
    let owner = GatewayDraftExecution::default();
    assert!(!owner.snapshot().warning);
    assert!(owner.approve_review());
    assert!(owner.start());
    owner.event(&PublicActionSessionEvent::HardwareApprovalStarted);
    assert_eq!(owner.snapshot().status, GatewayDraftStatus::Attention);
    owner.event(&PublicActionSessionEvent::HardwareApprovalFailed {
        message: "private diagnostic omitted".into(),
    });
    assert!(owner.snapshot().warning);
    owner.event(&PublicActionSessionEvent::HardwareApprovalStarted);
    assert!(!owner.snapshot().warning);
    owner.event(&PublicActionSessionEvent::HardwareApprovalCompleted);
    assert_eq!(owner.snapshot().status, GatewayDraftStatus::InProgress);
    owner.event(&PublicActionSessionEvent::AttemptHandoff {
        step: PublicActionProgressStep::Approve,
    });
    let reopened = owner.clone();
    owner.event(&PublicActionSessionEvent::StepFailed {
        step: PublicActionProgressStep::Shield,
        message: "private diagnostic omitted".into(),
    });
    assert!(reopened.snapshot().warning);
    owner.event(&PublicActionSessionEvent::FeeAuthorizationRequired {
        step: PublicActionProgressStep::Shield,
        max_fee_per_gas: 10,
        max_priority_fee_per_gas: 1,
        message: "private diagnostic omitted".into(),
    });
    // Progress and session events use separate channels; old progress may arrive late.
    owner.progress(&PublicActionProgressUpdate {
        step: PublicActionProgressStep::Approve,
        status: PublicActionProgressStatus::Pending,
        tx_hash: None,
        message: None,
    });
    assert_eq!(reopened.snapshot().status, GatewayDraftStatus::Attention);
    assert!(!reopened.snapshot().warning);
    assert_eq!(reopened.snapshot().step_label, "Shield");
    assert!(!reopened.snapshot().message.contains("private diagnostic"));
    owner.finish(true);
    owner.event(&PublicActionSessionEvent::HardwareApprovalFailed {
        message: "late".into(),
    });
    assert_eq!(reopened.snapshot().status, GatewayDraftStatus::Done);
    assert!(!reopened.snapshot().warning);
    assert!(!reopened.snapshot().can_retry);
}
