use super::*;

#[gpui::test]
fn stopping_a_tron_send_before_its_handoff_broadcasts_nothing_and_returns_to_the_form(
    cx: &mut TestAppContext,
) {
    use crate::root::public_action::PublicActionStepStatus;
    let path = temp_wallet_db_root("tron-send-stop");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let chain = Arc::new(TronSendChain {
        hang: true,
        ..TronSendChain::new(
            Ok(wallet_ops::tron::http_api::TronBroadcast {
                accepted: true,
                code: None,
            }),
            true,
        )
    });
    let intent = open_tron_send(&root, cx, &runtime, &chain);
    approve_tron_send(&root, cx, intent);
    // Authorized and signing-bound, not yet handed off.
    drive_until(cx, &runtime, |_| {
        chain
            .reached_signing
            .load(std::sync::atomic::Ordering::SeqCst)
    });
    root.update_in(cx, |root, window, cx| {
        root.stop_public_action_progress(cx);
        assert!(!root.public_form.sending);
        assert_eq!(
            root.public_form.action_progress[0].status,
            PublicActionStepStatus::Stopped
        );
        root.close_public_action_progress_dialog(window, cx);
        assert_eq!(root.public_form.action_progress.len(), 0);
        assert!(
            root.tron_public.send.form.is_some(),
            "the form stays open for editing"
        );
    });
    runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(50)));
    cx.run_until_parked();
    assert_eq!(chain.broadcasts(), Vec::<alloy::primitives::B256>::new());

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_tron_send_the_endpoint_failed_to_take_ends_only_from_solidified_blocks(
    cx: &mut TestAppContext,
) {
    use crate::root::public_action::PublicActionStepStatus;
    let path = temp_wallet_db_root("tron-send-outcome");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let cases = [
        // No answer, then a solidified block includes it.
        (
            Err(wallet_ops::RpcBrokerError::Timeout),
            true,
            PublicActionStepStatus::Done,
            None,
        ),
        // A refusal, then a solidified block passes its expiration without it.
        (
            Ok(wallet_ops::tron::http_api::TronBroadcast {
                accepted: false,
                code: Some("CONTRACT_VALIDATE_ERROR".to_owned()),
            }),
            false,
            PublicActionStepStatus::Error,
            Some("The transaction expired without being included. No TRX was spent."),
        ),
    ];
    for (broadcast, includes, status, summary) in cases {
        let chain = Arc::new(TronSendChain::new(broadcast, includes));
        let intent = open_tron_send(&root, cx, &runtime, &chain);
        approve_tron_send(&root, cx, intent);
        drive_until(cx, &runtime, |cx| {
            !root.read_with(cx, |root, _| root.public_form.sending)
        });
        root.read_with(cx, |root, _| {
            let step = &root.public_form.action_progress[0];
            assert_eq!(step.status, status, "{includes}");
            assert!(step.tx_hash.is_some(), "{includes}");
            assert_eq!(
                root.public_form.action_error_summary.as_deref(),
                summary,
                "{includes}"
            );
            assert_eq!(
                root.tron_public.send.completed.is_some(),
                includes,
                "{includes}"
            );
        });
        assert_eq!(chain.broadcasts().len(), 1, "{includes}");
    }

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn selecting_an_evm_chain_stops_a_tron_send_only_before_its_handoff(cx: &mut TestAppContext) {
    use crate::root::public_action::PublicActionStepStatus;
    use std::sync::atomic::Ordering::SeqCst;
    let path = temp_wallet_db_root("tron-send-chain-switch");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    root.update_in(cx, |root, _, _| use_public_only_test_chains(root));
    let accepted = || {
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        })
    };
    let sending =
        |cx: &mut gpui::VisualTestContext| root.read_with(cx, |root, _| root.public_form.sending);

    // Before its handoff, the switch stops the send and nothing is broadcast.
    let hung = Arc::new(TronSendChain {
        hang: true,
        ..TronSendChain::new(accepted(), true)
    });
    let intent = open_tron_send(&root, cx, &runtime, &hung);
    approve_tron_send(&root, cx, intent);
    drive_until(cx, &runtime, |_| hung.reached_signing.load(SeqCst));
    root.update_in(cx, |root, window, cx| {
        assert!(root.public_form.action_stop_available);
        root.select_chain(20, window, cx);
        assert_eq!(root.public_form.action_progress.len(), 0);
    });
    drive_until(cx, &runtime, |cx| !sending(cx));
    assert_eq!(hung.broadcasts(), Vec::<alloy::primitives::B256>::new());

    // After it, the send runs to its outcome under another EVM chain than it started on.
    let held = Arc::new(TronSendChain::new(accepted(), true));
    held.hold.store(true, SeqCst);
    let intent = open_tron_send(&root, cx, &runtime, &held);
    let uuid = intent.account_uuid.to_string();
    let scope = root.update_in(cx, |root, _, _| {
        root.activate_tron();
        let route = root.tron_public.route.clone().unwrap();
        root.public_balance_scope_for_route(route).unwrap()
    });
    approve_tron_send(&root, cx, intent);
    drive_until(cx, &runtime, |cx| {
        held.broadcasts().len() == 1
            && !root.read_with(cx, |root, _| root.public_form.action_stop_available)
    });
    root.update_in(cx, |root, window, cx| {
        // As if the handoff's event hadn't arrived yet: Stop and the switch still keep the send.
        root.public_form.action_stop_available = true;
        root.stop_public_action_progress(cx);
        assert!(root.public_form.sending);
        assert!(!root.public_form.action_stopped);
        root.select_chain(10, window, cx);
        assert!(root.public_form.sending);
        assert_eq!(
            root.public_form
                .action_progress
                .first()
                .map(|step| step.status),
            Some(PublicActionStepStatus::Pending)
        );
        assert!(
            root.tron_public
                .balance_cache
                .refreshing_accounts(&scope)
                .is_empty()
        );
        // The EVM chain's Shield can't take the slot the handed-off send still owns.
        let generation = root.public_form.action_generation;
        root.submit_public_shield_from_form(window, cx);
        assert!(root.public_form.sending);
        assert_eq!(
            root.public_form
                .action_progress
                .first()
                .map(|step| step.status),
            Some(PublicActionStepStatus::Pending)
        );
        assert_eq!(root.public_form.action_generation, generation);
    });
    held.hold.store(false, SeqCst);
    drive_until(cx, &runtime, |cx| !sending(cx));
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.selected_network, crate::root::SelectedNetwork::Evm(10));
        let step = &root.public_form.action_progress[0];
        assert_eq!(step.status, PublicActionStepStatus::Done);
        assert!(step.tx_hash.is_some());
        assert!(root.tron_public.send.completed.is_some());
        // The outcome fenced the sender and refreshes it.
        assert!(
            root.tron_public
                .balance_cache
                .refreshing_accounts(&scope)
                .contains(&uuid)
        );
        root.select_tron(window, cx);
        assert_eq!(
            root.public_form.action_progress[0].status,
            PublicActionStepStatus::Done
        );
        assert!(root.tron_send_progress_note().is_some());
    });
    assert_eq!(held.broadcasts().len(), 1);

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_running_shield_keeps_the_action_slot_from_a_tron_send_until_a_chain_switch_ends_it(
    cx: &mut TestAppContext,
) {
    use tokio::sync::mpsc::error::TryRecvError;
    use wallet_ops::{PublicActionProgressStep, PublicActionSessionEvent};
    let path = temp_wallet_db_root("tron-send-held-shield");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    root.update_in(cx, |root, _, _| use_public_only_test_chains(root));

    // A Shield is running: it owns the progress, its task and its command channel.
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = runtime.spawn(std::future::pending::<()>());
    let generation = root.update_in(cx, |root, _, _| {
        root.public_form.shielding = true;
        let generation = root.start_public_action_progress_with_steps(
            PublicActionMode::Shield,
            vec![
                PublicActionProgressStep::Approve,
                PublicActionProgressStep::Shield,
            ],
            "USDC".to_owned(),
            None,
            PublicAccountSource::Derived,
            Some(command_tx),
            None,
            true,
        );
        root.public_form.action_task_abort_handle = Some(task.abort_handle());
        generation
    });

    // Opening and submitting a ready Tron Send leaves the Shield alone.
    let chain = Arc::new(TronSendChain::new(
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        }),
        true,
    ));
    let intent = open_tron_send(&root, cx, &runtime, &chain);
    root.update_in(cx, |root, window, cx| {
        root.tron_public.send.form.as_mut().unwrap().review = Some(intent.review);
        root.submit_tron_send_from_form(window, cx);
        assert_eq!(root.public_form.action_progress.len(), 2);
        assert_eq!(root.public_form.action_generation, generation);
        assert!(root.public_form.action_task_abort_handle.is_some());
        assert!(!root.public_form.sending);
        assert!(root.public_form.shielding);
    });
    runtime.block_on(tokio::time::sleep(Duration::from_millis(10)));
    cx.run_until_parked();
    assert!(!task.is_finished());
    assert!(matches!(command_rx.try_recv(), Err(TryRecvError::Empty)));

    // Switching to an EVM chain ends the Shield and rejects its late events.
    root.update_in(cx, |root, window, cx| {
        root.select_chain(10, window, cx);
        assert!(!root.public_form.shielding);
        assert_eq!(root.public_form.action_progress.len(), 0);
        assert_ne!(root.public_form.action_generation, generation);
        root.apply_public_action_session_event(
            generation,
            PublicActionSessionEvent::AttemptRejected {
                step: PublicActionProgressStep::Shield,
                message: "late".to_owned(),
            },
            cx,
        );
        assert!(root.public_form.action_action_error.is_none());
    });
    assert!(runtime.block_on(task).unwrap_err().is_cancelled());
    assert!(matches!(
        command_rx.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
    assert_eq!(chain.broadcasts(), Vec::<alloy::primitives::B256>::new());

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
