use super::*;

#[test]
fn progress_ownership_transfers_from_startup_and_survives_ready_to_syncing() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let registration = lifecycle.prepare_startup(1);
    let startup_is_current = lifecycle.is_current_startup(
        registration.chain_id,
        registration.generation,
        registration.task_id,
    );

    assert!(chain_progress_update_is_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        startup_is_current,
        Some(ChainProgressProjection::Loading),
        &registration.observer_token,
    ));

    lifecycle.finish_startup_after_session_installation(
        1,
        registration.generation,
        registration.task_id,
    );
    assert!(!lifecycle.is_current_startup(1, registration.generation, registration.task_id,));

    for projection in [
        ChainProgressProjection::Ready {
            token: &registration.observer_token,
        },
        ChainProgressProjection::Syncing {
            token: &registration.observer_token,
        },
    ] {
        assert!(chain_progress_update_is_current(
            Some("wallet-1"),
            7,
            "wallet-1",
            7,
            1,
            false,
            Some(projection),
            &registration.observer_token,
        ));
    }

    let replacement = lifecycle.prepare_startup(1);
    lifecycle.finish_startup_after_session_installation(
        1,
        replacement.generation,
        replacement.task_id,
    );
    let replacement_projection = Some(ChainProgressProjection::Syncing {
        token: &replacement.observer_token,
    });
    assert!(!chain_progress_update_is_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        false,
        replacement_projection,
        &registration.observer_token,
    ));
    assert!(chain_progress_update_is_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        false,
        replacement_projection,
        &replacement.observer_token,
    ));
    assert!(!chain_progress_update_is_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        2,
        false,
        replacement_projection,
        &replacement.observer_token,
    ));
}

#[test]
fn replacement_observer_rejects_all_delayed_events_from_previous_observer() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let observer_a = lifecycle.prepare_startup(1);
    let observer_b = lifecycle.prepare_startup(1);
    assert_eq!(observer_a.generation, observer_b.generation);
    assert_ne!(observer_a.observer_token, observer_b.observer_token);
    let installed_observer = observer_b.observer_token.clone();
    lifecycle.finish_startup_after_session_installation(
        observer_b.chain_id,
        observer_b.generation,
        observer_b.task_id,
    );
    assert!(!lifecycle.is_current_startup(
        observer_b.chain_id,
        observer_b.generation,
        observer_b.task_id,
    ));

    let event_names = [
        "snapshot",
        "readiness",
        "sync tip",
        "POI refreshing",
        "POI artifact progress",
    ];
    for event_name in event_names {
        assert!(
            !installed_observer_is_exact_current(
                Some("wallet-1"),
                7,
                "wallet-1",
                7,
                1,
                Some(&installed_observer),
                &observer_a.observer_token,
            ),
            "delayed observer A {event_name} event must be rejected",
        );
        assert!(
            installed_observer_is_exact_current(
                Some("wallet-1"),
                7,
                "wallet-1",
                7,
                1,
                Some(&installed_observer),
                &observer_b.observer_token,
            ),
            "current observer B {event_name} event must be accepted",
        );
    }

    assert!(!installed_observer_is_exact_current(
        Some("wallet-2"),
        7,
        "wallet-1",
        7,
        1,
        Some(&installed_observer),
        &observer_b.observer_token,
    ));
    assert!(!installed_observer_is_exact_current(
        Some("wallet-1"),
        8,
        "wallet-1",
        7,
        1,
        Some(&installed_observer),
        &observer_b.observer_token,
    ));
    assert!(!installed_observer_is_exact_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        2,
        Some(&installed_observer),
        &observer_b.observer_token,
    ));
}

#[tokio::test]
async fn replacement_cancels_and_observes_superseded_installed_observer() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let first = lifecycle.prepare_startup(1);
    let mut first_observer = lifecycle.register_installed_observer(&first);

    let second = lifecycle.prepare_startup(1);
    first_observer
        .cancel_rx
        .changed()
        .await
        .expect("first observer cancellation");
    assert!(*first_observer.cancel_rx.borrow());
    let _ = first_observer.completed_tx.send(true);

    let mut second_observer = lifecycle.register_installed_observer(&second);
    let cleanup = lifecycle.supersede_wallet();
    second_observer
        .cancel_rx
        .changed()
        .await
        .expect("second observer cancellation");
    assert!(*second_observer.cancel_rx.borrow());

    let cleanup_join = tokio::spawn(cleanup.shutdown());
    tokio::task::yield_now().await;
    assert!(!cleanup_join.is_finished());
    let _ = second_observer.completed_tx.send(true);
    cleanup_join
        .await
        .expect("cleanup task")
        .expect("cleanup superseded observers");
}

#[tokio::test]
async fn auxiliary_stream_closure_does_not_close_authoritative_observation() {
    let (observation_tx, mut observation_rx) = tokio::sync::watch::channel(0_u64);
    let (auxiliary_tx, auxiliary_rx) = tokio::sync::watch::channel(false);
    let mut auxiliary_rx = Some(auxiliary_rx);
    drop(auxiliary_tx);

    let changed = auxiliary_rx
        .as_mut()
        .expect("auxiliary receiver")
        .changed()
        .await;
    assert!(!retain_auxiliary_stream(&mut auxiliary_rx, &changed));
    assert!(auxiliary_rx.is_none());

    observation_tx.send(1).expect("authoritative observation");
    observation_rx
        .changed()
        .await
        .expect("authoritative stream remains open");
    assert_eq!(*observation_rx.borrow(), 1);
}

#[test]
fn observation_closure_wins_over_ready_projection() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let observer = lifecycle.prepare_startup(1);

    let terminal = installed_observer_terminal_transition(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        Some(InstalledObserverProjection::Ready {
            token: &observer.observer_token,
            start_block: 123,
        }),
        &observer.observer_token,
        Arc::from("wallet session observation stream closed"),
    )
    .expect("observation closure must terminalize the exact ready observer");

    assert_eq!(
        terminal.message.as_ref(),
        "wallet session observation stream closed"
    );
    assert_eq!(terminal.start_block, 123);
}

#[test]
fn observation_closure_wins_over_syncing_projection() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let observer = lifecycle.prepare_startup(1);

    let terminal = installed_observer_terminal_transition(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        Some(InstalledObserverProjection::Syncing {
            token: &observer.observer_token,
            start_block: 456,
        }),
        &observer.observer_token,
        Arc::from("wallet session observation stream closed"),
    )
    .expect("observation closure must terminalize the exact syncing observer");

    assert_eq!(
        terminal.message.as_ref(),
        "wallet session observation stream closed"
    );
    assert_eq!(terminal.start_block, 456);
}

#[test]
fn authoritative_observation_closure_terminalizes_exact_installed_observer() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let observer = lifecycle.prepare_startup(1);

    let terminal = installed_observer_terminal_transition(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        Some(InstalledObserverProjection::Ready {
            token: &observer.observer_token,
            start_block: 789,
        }),
        &observer.observer_token,
        Arc::from("wallet session observation stream closed"),
    )
    .expect("observation closure must terminalize the exact observer");

    assert_eq!(
        terminal.message.as_ref(),
        "wallet session observation stream closed"
    );
    assert_eq!(terminal.start_block, 789);
}

#[test]
fn stale_stream_closure_does_not_replace_newer_observer() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let stale = lifecycle.prepare_startup(1);
    let current = lifecycle.prepare_startup(1);

    let terminal = installed_observer_terminal_transition(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        Some(InstalledObserverProjection::Ready {
            token: &current.observer_token,
            start_block: 999,
        }),
        &stale.observer_token,
        Arc::from("wallet session observation stream closed"),
    );

    assert_eq!(terminal, None);
}
