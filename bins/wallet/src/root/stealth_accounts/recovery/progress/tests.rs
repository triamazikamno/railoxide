use super::*;
use PublicActionStepStatus::{Done, Error, NotStarted, Pending, Stopped, Warning};
use TransactionGenerationStage::{
    GeneratingPoiProofs, ProvingTransaction, SelectingPrivateNotes, WaitingForBroadcasterResponse,
};

#[test]
fn recovery_stepper_retains_preparation_and_drains_failure_stage() {
    let mut preparation = RecoveryProgress::new(1, RecoveryProgressSource::Preparation, None);
    preparation.finish(Done, "Prepared".into());
    let (sender, receiver) = watch::channel(SelectingPrivateNotes);
    let mut progress = RecoveryProgress::new(
        2,
        RecoveryProgressSource::Broadcaster(receiver),
        Some(&preparation),
    );
    assert_eq!(progress.preparation.as_ref().unwrap().status, Done);

    // The worker can advance and fail before GPUI polls its watch receiver.
    sender.send(ProvingTransaction).unwrap();
    progress.finish(Error, "Proof generation failed".into());
    assert_eq!(progress.preparation.as_ref().unwrap().status, Done);
    assert_eq!(
        progress
            .steps
            .iter()
            .map(|step| step.status)
            .collect::<Vec<_>>(),
        [Done, Error, NotStarted, NotStarted, NotStarted, NotStarted]
    );
    assert_eq!(
        progress.steps[1].message.as_deref(),
        Some("Proof generation failed")
    );
}

#[test]
fn recovery_stepper_stops_at_latest_stage_and_resets_for_another_attempt() {
    let (sender, receiver) = watch::channel(SelectingPrivateNotes);
    let mut progress =
        RecoveryProgress::new(1, RecoveryProgressSource::Broadcaster(receiver), None);
    sender.send(GeneratingPoiProofs).unwrap();
    progress.finish(Stopped, "Stopped locally".into());
    assert_eq!(
        progress
            .steps
            .iter()
            .map(|step| step.status)
            .collect::<Vec<_>>(),
        [Done, Done, Done, Stopped, NotStarted, NotStarted]
    );

    let (sender, receiver) = watch::channel(SelectingPrivateNotes);
    let mut next = RecoveryProgress::new(
        2,
        RecoveryProgressSource::Broadcaster(receiver),
        Some(&progress),
    );
    assert_eq!(next.steps[0].status, Pending);
    assert!(
        next.steps[1..]
            .iter()
            .all(|step| step.status == NotStarted && step.message.is_none())
    );
    sender.send(WaitingForBroadcasterResponse).unwrap();
    next.finish(
        Warning,
        "Response timed out; execution is not confirmed".into(),
    );
    assert!(next.steps[..5].iter().all(|step| step.status == Done));
    assert_eq!(next.steps[5].status, Warning);
}

#[test]
fn native_recovery_stepper_keeps_confirmation_pending_until_effects_are_checked() {
    let (sender, receiver) = watch::channel(None);
    let mut progress = RecoveryProgress::new(1, RecoveryProgressSource::Native(receiver), None);
    let update = PublicActionProgressUpdate {
        step: wallet_ops::PublicActionProgressStep::Shield,
        status: PublicActionProgressStatus::Pending,
        tx_hash: Some("submitted transaction".into()),
        message: None,
    };
    sender.send(Some(update.clone())).unwrap();
    progress.update();
    assert_eq!(progress.steps[0].status, Done);
    assert_eq!(progress.steps[1].status, Pending);

    sender
        .send(Some(PublicActionProgressUpdate {
            status: PublicActionProgressStatus::Done,
            ..update
        }))
        .unwrap();
    progress.update();
    assert_eq!(progress.steps[1].status, Pending);
    // A later error can omit the hash; keep the submitted transaction available.
    sender
        .send(Some(PublicActionProgressUpdate {
            step: wallet_ops::PublicActionProgressStep::Shield,
            status: PublicActionProgressStatus::Error,
            tx_hash: None,
            message: Some("Recovery effects are missing".into()),
        }))
        .unwrap();
    progress.finish(Error, "Recovery effects are missing".into());
    assert_eq!(progress.steps[0].status, Done);
    assert_eq!(progress.steps[1].status, Error);
    assert_eq!(progress.tx_hash.as_deref(), Some("submitted transaction"));
}

struct ProgressTestWindow(RecoveryProgress);

impl gpui::Render for ProgressTestWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl gpui::IntoElement {
        div()
            .size_full()
            .p_3()
            .child(self.0.render_content().w_full())
    }
}

#[gpui::test]
fn recovery_stepper_renders_all_stages_at_desktop_and_narrow_widths(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let mut preparation = RecoveryProgress::new(1, RecoveryProgressSource::Preparation, None);
    preparation.finish(Done, "Recovery prepared.".into());
    let (sender, receiver) = watch::channel(SelectingPrivateNotes);
    let progress = RecoveryProgress::new(
        2,
        RecoveryProgressSource::Broadcaster(receiver),
        Some(&preparation),
    );
    let mut view = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|_| ProgressTestWindow(progress));
        view = Some(content.clone());
        gpui_component::Root::new(content, window, cx)
    });
    let view = view.unwrap();
    for (width, rem, hash_byte) in [(560., 16., 0x31), (360., 20., 0x42)] {
        let tx_hash = format!("{:#x}", alloy::primitives::B256::repeat_byte(hash_byte));
        cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(1200.)));
        for stage in [ProvingTransaction, WaitingForBroadcasterResponse] {
            sender.send(stage).unwrap();
            cx.update(|window, cx| {
                window.set_rem_size(gpui::px(rem));
                view.update(cx, |view, cx| {
                    view.0.update();
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            let ids = view.read_with(cx, |view, _| {
                std::iter::once("submission-step-stealth-recovery-preparation-error".to_owned())
                    .chain(view.0.steps.iter().map(|step| {
                        format!(
                            "submission-step-stealth-recovery-{}-error",
                            private_broadcaster_stage_id(step.stage)
                        )
                    }))
                    .collect::<Vec<_>>()
            });
            let mut previous_bottom = gpui::px(0.);
            for id in ids {
                let bounds = cx
                    .debug_bounds(id.leak())
                    .expect("each recovery stage remains visible");
                assert!(bounds.left() >= gpui::px(0.) && bounds.right() <= gpui::px(width));
                assert!(
                    bounds.top() >= previous_bottom,
                    "step rows must not overlap: {bounds:?}"
                );
                assert!(
                    bounds.size.height >= gpui::px(rem * 2.),
                    "step text must not collapse: {bounds:?}"
                );
                previous_bottom = bounds.bottom();
            }
        }
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.0
                    .finish_broadcaster(PublicBroadcasterResultKind::Submitted {
                        tx_hash: tx_hash.clone(),
                    });
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
        let row = cx
            .debug_bounds("stealth-recovery-progress-tx-hash")
            .expect("submitted recovery uses the shared transaction hash row");
        assert!(row.left() >= gpui::px(0.) && row.right() <= gpui::px(width));
        // Clipboard has no debug selector; its button occupies the row's trailing edge.
        cx.simulate_click(
            gpui::point(row.right() - gpui::px(2.), row.top() + gpui::px(2.)),
            gpui::Modifiers::none(),
        );
        assert_eq!(
            cx.read_from_clipboard().unwrap().text(),
            Some(tx_hash.clone())
        );

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.0 = RecoveryProgress::new(
                    3,
                    RecoveryProgressSource::Broadcaster(sender.subscribe()),
                    Some(&preparation),
                );
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("stealth-recovery-progress-tx-hash")
                .is_none()
        );
    }
}
