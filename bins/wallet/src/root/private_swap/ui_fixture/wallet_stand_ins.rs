#[allow(clippy::wildcard_imports)]
use super::*;

/// 1.0 of `chain_id`'s wrapped native token `wrapped_native`, which stands in for a private
/// asset the wallet doesn't hold.
fn stand_in_asset(chain_id: u64, wrapped_native: Address) -> crate::root::UnshieldAsset {
    let amount = U256::from(10).pow(U256::from(18));
    crate::root::UnshieldAsset {
        chain_id,
        token: wrapped_native,
        label: known_tokens_for_chain(chain_id)
            .find(|token| token.token == wrapped_native)
            .map_or("WETH", |token| token.symbol)
            .to_owned(),
        decimals: Some(18),
        total: amount,
        poi_verified_total: amount,
        max_batched: amount,
        icon_path: None,
    }
}

/// The stand-in asset of a `max:` mode: 12.345678901234567891 of `chain_id`'s wrapped native
/// token `wrapped_native`, with its icon, in notes whose largest spend is 7.123456789012345678.
/// Send, Unshield and the swap's Sell panel offer it. `None` in every other mode.
pub(in crate::root::private_swap) fn max_asset(
    chain_id: u64,
    wrapped_native: Address,
) -> Option<crate::root::UnshieldAsset> {
    let total = U256::from(12_345_678_901_234_567_891_u64);
    matches!(mode(), Some(Mode::Max(_))).then(|| crate::root::UnshieldAsset {
        total,
        poi_verified_total: total,
        max_batched: U256::from(7_123_456_789_012_345_678_u64),
        icon_path: token_icon_asset_path(chain_id, &wrapped_native).map(WalletIconSource::embedded),
        ..stand_in_asset(chain_id, wrapped_native)
    })
}

/// What a `max:locked` mode reports locked of `token`, 0.5 when it is the stand-in Sell asset
/// `wrapped_native`. `None` in every other mode.
pub(in crate::root::private_swap) fn locked_sell_value(
    token: Address,
    wrapped_native: Option<Address>,
) -> Option<U256> {
    (matches!(mode(), Some(Mode::Max(Max::Locked))) && wrapped_native == Some(token))
        .then_some(U256::from(500_000_000_000_000_000_u64))
}

/// The stand-in asset of the `unshield`, a `max:`, a `delivery:` or a `hints:` mode, a `max:`
/// or `delivery:` one with its token's icon. `None` in every other mode.
pub(in crate::root) fn unshield_asset(
    chain_id: u64,
    wrapped_native: Address,
) -> Option<crate::root::UnshieldAsset> {
    max_asset(chain_id, wrapped_native).or_else(|| match mode()? {
        Mode::Unshield | Mode::Hints(_) => Some(stand_in_asset(chain_id, wrapped_native)),
        Mode::Delivery(_) => send_asset(chain_id, wrapped_native),
        _ => None,
    })
}

/// The stand-in asset Send offers in a `broadcaster-progress:`, `broadcaster-review`, `max:` or
/// `delivery:` mode, with its token's icon. `None` in every other mode.
pub(in crate::root) fn send_asset(
    chain_id: u64,
    wrapped_native: Address,
) -> Option<crate::root::UnshieldAsset> {
    max_asset(chain_id, wrapped_native).or_else(|| {
        matches!(
            mode(),
            Some(Mode::BroadcasterProgress(_) | Mode::BroadcasterReview | Mode::Delivery(_))
        )
        .then(|| crate::root::UnshieldAsset {
            icon_path: token_icon_asset_path(chain_id, &wrapped_native)
                .map(WalletIconSource::embedded),
            ..stand_in_asset(chain_id, wrapped_native)
        })
    })
}

/// Whether the `broadcaster-review` mode is active, whose Send reviews prove and send nothing.
pub(in crate::root) fn holds_send_review() -> bool {
    matches!(mode(), Some(Mode::BroadcasterReview))
}

/// Whether a `delivery:` or `hints:` mode is active, whose Send and Unshield forms hold their
/// stand-in state: the forms' scheduled broadcaster and sponsorship estimates don't run, sync
/// snapshots leave the forms alone, and an approved Send or Unshield stops before executor
/// account preparation, proof generation or submission.
pub(in crate::root) fn holds_delivery() -> bool {
    matches!(mode(), Some(Mode::Delivery(_) | Mode::Hints(_)))
}

/// The token the Private tab's Swap offers in a `setup:`, `retry-review`, `native-buy` or `max:`
/// mode, `wrapped_native`, so an unfunded wallet can open the mode's swap. `None` in every other
/// mode.
pub(in crate::root) fn setup_swap_token(wrapped_native: Address) -> Option<Address> {
    matches!(
        mode(),
        Some(
            Mode::Setup(_) | Mode::RetryReview | Mode::NativeBuy | Mode::Max(_) | Mode::NonEvm(..)
        )
    )
    .then_some(wrapped_native)
}

/// What New Tor session stages in a `rotation:` mode: `Some(true)` when the broadcaster
/// network fails after the commit, `Some(false)` when the rotation stays pending.
pub(in crate::root) fn tor_rotation() -> Option<bool> {
    match mode()? {
        Mode::Rotation { waku_failed } => Some(waku_failed),
        _ => None,
    }
}

/// Whether the UTXO table shows the rows of the `utxos` mode in place of the wallet's own,
/// whatever the network's sync state.
pub(in crate::root) fn shows_utxos() -> bool {
    matches!(mode(), Some(Mode::Utxos))
}

/// In the `logs` mode, pushes the mode's 300 lines into `logs`, once per process.
pub(in crate::root) fn fill_logs(logs: &ui::logs::LogStore) {
    use tracing::Level;
    const LINES: [(Level, &str, &str); 12] = [
        (
            Level::INFO,
            "arti_client::client",
            "Connected to the Tor network",
        ),
        (
            Level::DEBUG,
            "arti_client::dirmgr",
            "Loaded 7,203 microdescriptors from the directory cache",
        ),
        (
            Level::TRACE,
            "arti_client::status",
            "Bootstrap status: 100% done",
        ),
        (
            Level::WARN,
            "arti_client::status",
            "Circuit build timed out after 60 s on guard $A3F2…; retrying with another guard, which can take a while on a slow or filtered network",
        ),
        (
            Level::INFO,
            "wallet_ops::rpc_broker",
            "eth_blockNumber answered by https://eth.example/ in 182 ms",
        ),
        (
            Level::WARN,
            "wallet_ops::rpc_broker",
            "https://rpc.example/ timed out after 10 s on eth_getLogs for blocks 21480000..21480215; trying the next endpoint for chain 1",
        ),
        (
            Level::DEBUG,
            "wallet_ops::rpc_broker::warmup",
            "Warming 3 endpoints for chain 42161",
        ),
        (
            Level::TRACE,
            "wallet_ops::rpc_broker_metrics",
            "latency p50=140ms p95=610ms over 64 requests",
        ),
        (
            Level::INFO,
            "wallet_ops::poi",
            "Submitted proofs of innocence for 2 outputs",
        ),
        (
            Level::ERROR,
            "wallet_ops::poi",
            "https://poi.example/ refused the proof submission: list 0x55ae…0aa1 is not yet synced; the outputs stay pending and the request is retried in 5 minutes",
        ),
        (
            Level::DEBUG,
            "wallet_ops::poi",
            "Fetched 128 blinded commitments",
        ),
        (
            Level::INFO,
            "wallet_ops::rpc_broker",
            "Chain 8453 head 24,118,307 from https://base.example/",
        ),
    ];
    const COUNT: u64 = 300;
    static FILLED: AtomicBool = AtomicBool::new(false);
    if !matches!(mode(), Some(Mode::Logs)) || FILLED.swap(true, Ordering::Relaxed) {
        return;
    }
    let start = ui::logs::now_unix_ms().saturating_sub(COUNT * 1_000);
    for (ix, (level, target, message)) in (0..COUNT).zip(LINES.into_iter().cycle()) {
        logs.push(ui::logs::LogEntry {
            seq: 0,
            unix_ms: start + ix * 1_000,
            level,
            target: target.into(),
            message: message.into(),
        });
    }
}

/// The `tron:` mode, whose Tron accounts, reads and send progress are staged.
pub(in crate::root) fn tron() -> Option<Tron> {
    match mode()? {
        Mode::Tron(tron) => Some(tron),
        _ => None,
    }
}

/// Whether the Derive dialog treats the selected wallet as Ledger-derived and asks the
/// `hardware-derive` mode's stand-in Ledger.
pub(in crate::root) fn hardware_derive() -> bool {
    matches!(mode(), Some(Mode::HardwareDerive))
}

/// Whether the `walletconnect-request` mode stages its canned dapp request.
pub(in crate::root) fn walletconnect_request() -> bool {
    matches!(mode(), Some(Mode::WalletConnectRequest))
}

/// The result an `update-check:` mode shows in place of a check, which sends nothing. The
/// canned releases are placed against this build's version. `None` in every other mode.
pub(in crate::root) fn update_check() -> Option<Result<ReleaseTimeline, ReleaseCheckError>> {
    const NEWEST_NOTES: &str = r#"## Highlights

- **Update check**: see whether a newer release is out, from the build label.
- Faster private balance sync after the wallet wakes from sleep.
- Fixed a crash when a chain's RPC endpoint returned an empty block.

![Release screenshot](https://github.com/triamazikamno/railoxide/raw/main/docs/screenshot.png)

Verify the download before installing:

```sh
sha256sum -c railoxide-checksums.txt
```

<details><summary>Packaging</summary><a href="https://example.com/packages">All packages</a></details>

Full changelog: [compare](https://github.com/triamazikamno/railoxide/compare/v0.0.0...v0.0.1) and https://github.com/triamazikamno/railoxide/releases"#;
    const PATCH_NOTES: &str = "### Fixes\n\n1. Swaps no longer wait for a quote that already \
        expired.\n2. The UTXO table keeps its scroll position after a sync.\n\nRun \
        `railoxide --version` to see the installed build.";
    const SECOND_PATCH_NOTES: &str = "- Tor circuits for one-off requests close as soon as \
        the request ends.\n- <b>Hardware wallets</b>: Trezor passphrase prompts show the \
        device name.";
    const OLDER_NOTES: &str = "- Improves note scanning on large wallets.\n- Fixes the \
        Activity tab's unread dot after a restart.";

    let Mode::UpdateCheck(state) = mode()? else {
        return None;
    };
    if state == UpdateCheck::Failed {
        return Some(Err(ReleaseCheckError {
            reason: "GitHub returned 403 Forbidden.".to_owned(),
        }));
    }
    let part = |value: &str| value.parse::<u64>().unwrap_or(0);
    let major = part(env!("CARGO_PKG_VERSION_MAJOR"));
    let minor = part(env!("CARGO_PKG_VERSION_MINOR"));
    let patch = part(env!("CARGO_PKG_VERSION_PATCH"));
    let release = |tag: String, name: Option<&str>, day: &str, body: Option<&str>| GithubRelease {
        html_url: format!("{RAILOXIDE_REPOSITORY_URL}/releases/tag/{tag}"),
        name: Some(name.map_or_else(|| tag.clone(), str::to_owned)),
        tag_name: tag,
        body: body.map(str::to_owned),
        published_at: Some(format!("2026-{day}T12:00:00Z")),
        prerelease: false,
        draft: false,
    };
    // shortcut: older releases sit one minor version below the build, so they order older
    // only while the build's minor version is above 0.
    let mut releases: Vec<GithubRelease> = (1..=11_u64)
        .map(|index| {
            release(
                format!("v{major}.{}.{index}", minor.saturating_sub(1)),
                None,
                &format!("09-{:02}", 2 * index + 1),
                (index % 3 != 0).then_some(OLDER_NOTES),
            )
        })
        .collect();
    if state != UpdateCheck::Ahead {
        releases.push(release(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            None,
            "10-02",
            Some(OLDER_NOTES),
        ));
    }
    if state == UpdateCheck::Behind {
        let newest = format!("v{major}.{}.0", minor + 1);
        releases.extend([
            release(
                format!("v{major}.{minor}.{}", patch + 1),
                None,
                "10-12",
                Some(PATCH_NOTES),
            ),
            release(
                newest.clone(),
                Some(&format!("Railoxide {newest}")),
                "10-30",
                Some(NEWEST_NOTES),
            ),
            release(
                format!("v{major}.{minor}.{}", patch + 2),
                None,
                "10-20",
                Some(SECOND_PATCH_NOTES),
            ),
            GithubRelease {
                prerelease: true,
                ..release(
                    format!("v{major}.{}.1-rc.1", minor + 1),
                    None,
                    "11-02",
                    Some(OLDER_NOTES),
                )
            },
        ]);
    }
    Some(Ok(classify(env!("CARGO_PKG_VERSION"), releases)))
}

/// The finality the `utxos` mode counts from its own head block, in place of the network's.
/// `None` in every other mode.
pub(in crate::root) fn utxo_finality_context() -> Option<UtxoFinalityContext> {
    shows_utxos().then_some(UtxoFinalityContext::new(
        Some(UTXO_HEAD),
        Some(UTXO_HEAD - UTXO_FINALITY_DEPTH),
        Some(UTXO_FINALITY_DEPTH),
    ))
}

/// The rows of the `utxos` mode, newest first, with ages from the time of the call. They take
/// the USDC and WETH of `chain_id`, or Ethereum's where it lists none. `None` in every other
/// mode, or without either token.
pub(in crate::root) fn utxo_rows(chain_id: u64) -> Option<Vec<UtxoDisplayRow>> {
    const HOUR: u64 = 3_600;
    const DAY: u64 = 24 * HOUR;
    /// 123,456.789012345678901234 units of an 18-decimal token.
    const LONG_AMOUNT: u128 = 123_456_789_012_345_678_901_234;
    if !shows_utxos() {
        return None;
    }
    let token = |symbol: &str| {
        [chain_id, 1]
            .into_iter()
            .find_map(|chain| known_tokens_for_chain(chain).find(|token| token.symbol == symbol))
    };
    let (usdc, weth) = (token("USDC")?, token("WETH")?);
    let now = now_unix();
    let ago = |seconds: u64| now.saturating_sub(seconds);
    let hash = |seed: &str| Some(alloy::hex::encode_prefixed(keccak256(seed)));
    let private = || "Private Output".to_owned();
    Some(vec![
        // Received 3 blocks below the head, with 3 of its 12 finality blocks passed.
        UtxoDisplayRow {
            activity_classification: private(),
            poi_status: "Pending receive".to_owned(),
            ppoi_state: UtxoPpoiState::Unknown,
            poi_spendable: false,
            pending_new: true,
            ..utxo_row(usdc, 250_000_000, 65_020, UTXO_HEAD - 3, ago(36))
        },
        // Spent 7 blocks below the head, with 7 of its 12 finality blocks passed.
        UtxoDisplayRow {
            poi_status: "Pending spend".to_owned(),
            spent_tx_hash: hash("ui-fixture-utxo-spend-65019"),
            spent_block_number: Some(UTXO_HEAD - 7),
            pending_spent: true,
            ..utxo_row(
                usdc,
                1_500_000_000,
                65_019,
                UTXO_HEAD - 3_300,
                ago(11 * HOUR),
            )
        },
        // A Shield 20 minutes old, whose PPOI is due in about 40 minutes.
        UtxoDisplayRow {
            poi_status: "Missing".to_owned(),
            ppoi_state: UtxoPpoiState::Missing,
            poi_spendable: false,
            ..utxo_row(
                weth,
                200_000_000_000_000_000,
                65_018,
                UTXO_HEAD - 100,
                ago(1_200),
            )
        },
        // A proof submitted 23 minutes ago and not verified yet, which can be retried.
        UtxoDisplayRow {
            activity_classification: private(),
            poi_status: "ProofSubmitted".to_owned(),
            ppoi_state: UtxoPpoiState::ProofSubmitted,
            ppoi_last_submission_at: Some(ago(1_380)),
            poi_spendable: false,
            ..utxo_row(usdc, 42_500_000, 65_017, UTXO_HEAD - 600, ago(2 * HOUR))
        },
        utxo_row(
            usdc,
            1_000_000_000,
            65_016,
            UTXO_HEAD - 4_100,
            ago(14 * HOUR),
        ),
        // A blocked Shield whose origin can refund it. It has no UTXO ID, so Refund does
        // nothing.
        UtxoDisplayRow {
            activity_classification: "BlockedShield".to_owned(),
            poi_status: "ShieldBlocked".to_owned(),
            ppoi_state: UtxoPpoiState::ShieldBlocked,
            poi_spendable: false,
            blocked_shield_rescue: Some(BlockedShieldRescueInfo {
                eligible: true,
                disabled_reason: None,
                origin_address: Some(Address::repeat_byte(0x4e).to_checksum(None)),
                public_account_uuid: Some("ui-fixture".to_owned()),
                public_account_label: Some("Savings".to_owned()),
            }),
            ..utxo_row(
                usdc,
                5_000_000_000,
                65_015,
                UTXO_HEAD - 7_800,
                ago(26 * HOUR),
            )
        },
        // Every decimal of an 18-decimal amount, under a long symbol.
        UtxoDisplayRow {
            token: "stkWETH-USDC-LPv2".to_owned(),
            amount: group_thousands(&format_scaled_amount(
                U256::from(LONG_AMOUNT),
                weth.decimals,
            )),
            activity_classification: private(),
            ..utxo_row(weth, LONG_AMOUNT, 65_014, UTXO_HEAD - 21_600, ago(3 * DAY))
        },
        UtxoDisplayRow {
            spent_tx_hash: hash("ui-fixture-utxo-spend-65013"),
            spent_block_number: Some(UTXO_HEAD - 14_400),
            is_spent: true,
            ..utxo_row(
                weth,
                500_000_000_000_000_000,
                65_013,
                UTXO_HEAD - 43_200,
                ago(6 * DAY),
            )
        },
        // A Shield whose PPOI is taking longer than the usual hour.
        UtxoDisplayRow {
            poi_status: "Missing".to_owned(),
            ppoi_state: UtxoPpoiState::Missing,
            poi_spendable: false,
            ..utxo_row(
                weth,
                300_000_000_000_000_000,
                65_012,
                UTXO_HEAD - 50_400,
                ago(7 * DAY),
            )
        },
    ])
}

/// An unspent, verified Shield of `raw` units of `token` at `position` in tree 1, made in
/// `block` at `timestamp`, which each row of the `utxos` mode changes as it needs.
fn utxo_row(
    token: KnownTokenInfo,
    raw: u128,
    position: u64,
    block: u64,
    timestamp: u64,
) -> UtxoDisplayRow {
    let raw_value = U256::from(raw);
    UtxoDisplayRow {
        utxo_id: None,
        tree_position: format!("1/{position}"),
        token: token.symbol.to_owned(),
        token_icon_path: token_icon_asset_path(token.chain_id, &token.token)
            .map(WalletIconSource::embedded),
        amount: format_token_amount(raw_value, token.decimals),
        raw_value: Some(raw_value),
        activity_classification: "Shield".to_owned(),
        poi_status: "Valid".to_owned(),
        ppoi_state: UtxoPpoiState::Valid,
        ppoi_last_submission_at: None,
        poi_spendable: true,
        source_tx_hash: alloy::hex::encode_prefixed(keccak256(format!(
            "ui-fixture-utxo-{position}"
        ))),
        source_block_number: block,
        source_block_timestamp: timestamp,
        spent_tx_hash: None,
        spent_block_number: None,
        token_address: token.token.to_checksum(None),
        is_spent: false,
        pending_new: false,
        pending_spent: false,
        local_pending_spent: false,
        blocked_shield_rescue: None,
    }
}
