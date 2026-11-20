use super::*;
use wallet_ops::settings::build_effective_chain_configs;

/// A fee in another token names the approved limit in the approved token and the token the
/// setup pays in, and both fee messages name the control that reviews the new limit.
#[test]
fn setup_fee_messages_name_the_approved_token_and_the_reviewing_control() {
    let (approved, paid) = (Address::repeat_byte(0xaa), Address::repeat_byte(0xbb));
    // Without a token registry an amount names its token by address.
    let label = |token| railgun_ui::short_address(&token);
    for control in ["Continue", "Retry setup"] {
        assert_eq!(
            setup_fee_token_changed(1, None, (approved, U256::from(520_000)), paid, control),
            format!(
                "Setup fee token changed. Approved: up to 520000 raw token units ({}). This setup pays in {}. {control} to review the new fee limit.",
                label(approved),
                label(paid)
            )
        );
        let increased = setup_fee_increased(
            1,
            None,
            approved,
            (U256::from(520_000), U256::from(600_000)),
            control,
        );
        assert!(
            increased.ends_with(&format!(
                "exceeding the maximum by 80000 raw token units ({}). {control} to review the new fee limit.",
                label(approved)
            )),
            "{increased}"
        );
    }
}

#[test]
fn swap_entry_is_hidden_without_a_profile_and_explains_why_a_token_is_disabled() {
    let chains = build_effective_chain_configs(&wallet_ops::settings::WalletSettings::default())
        .expect("built-in chains");
    let profile = chains
        .get(1)
        .and_then(wallet_ops::settings::EffectiveChainConfig::swap_profile)
        .expect("built-in mainnet swap profile");
    let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse()
        .expect("WETH");

    for token in [weth, Address::repeat_byte(0x42)] {
        assert_eq!(
            swap_entry_availability(Some(&profile), Some(token), true, true),
            Some(Ok(()))
        );
    }
    // Native assets, empty balances and a stopped session keep the action visible but
    // disabled with a reason.
    for (token, spendable, active) in [
        (Address::ZERO, true, true),
        (weth, false, true),
        (weth, true, false),
    ] {
        assert!(matches!(
            swap_entry_availability(Some(&profile), Some(token), spendable, active),
            Some(Err(_))
        ));
    }
    assert_eq!(swap_entry_availability(None, Some(weth), true, true), None);
}

#[test]
fn bridge_polls_back_off_to_a_capped_interval() {
    assert_eq!(bridge_poll_interval(0), SWAP_OBSERVATION_INTERVAL);
    assert_eq!(bridge_poll_interval(1), SWAP_OBSERVATION_INTERVAL * 2);
    assert_eq!(bridge_poll_interval(5), MAX_BRIDGE_POLL_INTERVAL);
    assert_eq!(bridge_poll_interval(u32::MAX), MAX_BRIDGE_POLL_INTERVAL);
}

/// Two swaps tracked together are each polled only once due, a fresh 9-15 s after their
/// previous poll or first sight, so never on a shared tick; catch-up makes one due at once.
#[test]
fn swaps_are_polled_out_of_step_within_their_intervals() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scripted() -> Duration {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        const SECS: [u64; 5] = [9, 14, 15, 11, 12];
        Duration::from_secs(SECS[NEXT.fetch_add(1, Ordering::Relaxed) % SECS.len()])
    }
    let bounds = Duration::from_secs(9)..=Duration::from_secs(15);
    for _ in 0..1_000 {
        assert!(bounds.contains(&swap_poll_interval()));
    }

    let base = Instant::now();
    let mut schedule = SwapSchedule::with_interval(scripted, base);
    let mut polls = BTreeMap::<char, Vec<Duration>>::new();
    // The loop is driven once a second; a swap's page runs only when it is due.
    for second in 0..=60 {
        let now = base + Duration::from_secs(second);
        let due = schedule.poll_due(vec!['a', 'b'], |swap| *swap, now);
        assert!(due.len() <= 1, "both swaps polled at {second} s");
        for swap in due {
            polls.entry(swap).or_default().push(now - base);
        }
        assert!(schedule.next_due().is_some_and(|next| next > now));
    }
    assert_ne!(polls.get(&'a'), polls.get(&'b'));
    for (swap, times) in &polls {
        assert!(times.len() >= 4, "{swap}: {times:?}");
        let mut previous = Duration::ZERO;
        for &at in times {
            assert!(
                bounds.contains(&at.saturating_sub(previous)),
                "{swap}: {times:?}"
            );
            previous = at;
        }
    }

    // A lagging swap is due again at once; the other keeps its schedule.
    let now = base + Duration::from_secs(61);
    schedule.wake('b', now);
    assert_eq!(schedule.poll_due(vec!['a', 'b'], |swap| *swap, now), ['b']);
}

/// Swaps loaded with the view are first due 9-15 s after it opened, and later swaps 9-15 s
/// after they are first seen. A poll slower than its interval waits a fresh one from its
/// completion, unless the swap was woken while it ran.
#[test]
fn swaps_wait_from_their_poll_completion_and_loaded_swaps_from_the_start() {
    let secs = Duration::from_secs;
    let within = |at: Instant, from: Instant| (from + secs(9)..=from + secs(15)).contains(&at);
    let start = Instant::now();
    let mut schedule = SwapSchedule::with_interval(swap_poll_interval, start);
    assert_eq!(
        schedule.poll_due(vec!['a'], |swap| *swap, start + secs(5)),
        [] as [char; 0]
    );
    assert!(within(schedule.due[&'a'], start));
    let later = start + secs(8);
    assert_eq!(
        schedule.poll_due(vec!['a', 'b'], |swap| *swap, later),
        [] as [char; 0]
    );
    assert!(within(schedule.due[&'b'], later));

    let polled = BTreeSet::from(['a']);
    let t0 = start + secs(15);
    assert!(
        schedule
            .poll_due(vec!['a', 'b'], |swap| *swap, t0)
            .contains(&'a')
    );
    let done = t0 + secs(20);
    schedule.polled(&polled, done);
    assert!(
        !schedule
            .poll_due(vec!['a', 'b'], |swap| *swap, done)
            .contains(&'a')
    );
    assert!(within(schedule.due[&'a'], done));

    let t1 = schedule.due[&'a'];
    assert!(
        schedule
            .poll_due(vec!['a', 'b'], |swap| *swap, t1)
            .contains(&'a')
    );
    schedule.wake('a', t1 + secs(1));
    schedule.polled(&polled, t1 + secs(20));
    assert!(
        schedule
            .poll_due(vec!['a', 'b'], |swap| *swap, t1 + secs(20))
            .contains(&'a')
    );
}

/// A background read installs the route it opened only through the swap's current opening:
/// neither a read over a route Retry then dropped, nor an opening Retry interrupted, brings
/// a route back.
#[test]
fn background_reads_keep_only_a_route_opened_through_the_current_opening() {
    let client = || {
        CowOrderbookClient::new(
            wallet_ops::OperationHttpClient::for_tests(
                reqwest::Client::new(),
                wallet_ops::OperationNetworkIsolation::Dedicated,
            ),
            "http://127.0.0.1:1".parse().unwrap(),
            1,
        )
        .unwrap()
    };
    let mut tracking = SwapTracking::default();
    tracking.set_route(Some(client()));
    let kept = tracking.route_cell();
    tracking.set_route(None);
    tracking.keep_route(&kept);
    assert!(tracking.orderbook.is_none());

    let interrupted = tracking.route_cell();
    assert!(interrupted.set(client()).is_ok());
    tracking.set_route(None);
    tracking.keep_route(&interrupted);
    assert!(tracking.orderbook.is_none());

    let current = tracking.route_cell();
    assert!(current.set(client()).is_ok());
    tracking.keep_route(&current);
    assert!(tracking.orderbook.is_some());
}
