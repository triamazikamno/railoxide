#[allow(clippy::wildcard_imports)]
use super::*;

/// The stealth accounts an `accounts`, `refund:stealth` or `setup:replaced` mode shows in
/// place of the wallet's own, synthesized once. `None` in every other mode.
pub(in crate::root) fn stealth_accounts() -> Option<Vec<ExecutorRecord>> {
    static ACCOUNTS: OnceLock<Vec<ExecutorRecord>> = OnceLock::new();
    if matches!(mode(), Some(Mode::Setup(Setup::Replaced))) {
        return replaced_accounts().map(|(swap, retired)| vec![swap.clone(), retired.clone()]);
    }
    matches!(mode(), Some(Mode::Accounts | Mode::Refund(Refund::Stealth))).then(|| {
        ACCOUNTS
            .get_or_init(|| {
                let accounts = staged_accounts();
                if accounts.len() != 3 {
                    tracing::warn!(target: LOG_TARGET,"the debug UI fixture couldn't synthesize its stealth accounts");
                }
                accounts
            })
            .clone()
    })
}

/// The payload `hash` signed at `nonce`, handed off in `transaction` when it has one. Its
/// calldata does not decode, so each payload is an action of its own.
pub(super) fn staged_payload(
    nonce: u64,
    hash: u8,
    purpose: ExecutorPayloadPurpose,
    transaction: Option<u8>,
) -> Option<serde_json::Value> {
    let signed = BlockNumHash::new(402_118_900, B256::repeat_byte(0xb4));
    let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
        U256::from(nonce),
        Address::repeat_byte(0x5d),
        B256::repeat_byte(hash),
        purpose,
        ExecutorPayloadContext::new(
            Bytes::new(),
            ExecutorNonceObservation::new(signed, U256::from(nonce)),
            Vec::new(),
        ),
    ))
    .ok()?;
    payload["transaction_hashes"] = serde_json::json!(
        transaction
            .map(B256::repeat_byte)
            .into_iter()
            .collect::<Vec<_>>()
    );
    Some(payload)
}

/// The accounts of the `accounts` mode, decoded from the JSON a stored record has. An
/// account that fails to decode is left out.
fn staged_accounts() -> Vec<ExecutorRecord> {
    use ExecutorPayloadPurpose::{Operation, Recovery, SwapPreHook};
    const READ_AT: u64 = 402_119_640;
    let now = now_unix();
    let delegate = Address::repeat_byte(0x5d);
    let account = |index: u32,
                   address: u8,
                   consumed: u64,
                   issued: Vec<Option<serde_json::Value>>|
     -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
            "operation": ExecutorOperationId::random().ok()?, "index": index,
            "address": Address::repeat_byte(address), "delegate": delegate, "retired": false,
            "created_at": now.saturating_sub(3_600),
            "issued": issued.into_iter().collect::<Option<Vec<_>>>()?,
            "nonce_watermark": ExecutorNonceWatermark::new(U256::from(consumed), READ_AT),
        }))
    };
    // The swap's order expired, with its pre-hook recorded as executed at nonce 1.
    let swap = || {
        let mut uid = [0_u8; 56];
        uid[52..].copy_from_slice(&u32::try_from(now.saturating_sub(600)).ok()?.to_be_bytes());
        let mut swap = account(
            7,
            0x3b,
            3,
            vec![
                staged_payload(0, 0xb0, Operation, Some(0xc0)),
                staged_payload(1, 0xb1, SwapPreHook, None),
                staged_payload(1, 0xb2, Recovery, Some(0xc2)),
                staged_payload(2, 0xb3, Recovery, Some(0xc3)),
                staged_payload(2, 0xb4, Recovery, None),
                staged_payload(3, 0xb5, Recovery, Some(0xc5)),
            ],
        )?;
        swap["swap"] = serde_json::json!({
            "terms": SwapTerms::new(
                Address::repeat_byte(0x11),
                Address::ZERO,
                SwapRecipient::new(U256::ONE, [0; 32]),
                B256::repeat_byte(0xb0),
            ),
            "proof": SwapProof::new(B256::repeat_byte(0xb6), Vec::new()),
            "orders": [{
                "attempt": 0, "uid": alloy::hex::encode_prefixed(uid),
                "delivery": SwapDelivery::External { receiver: Address::repeat_byte(0x42) },
                "bounds": {
                    "sell_amount": "0x5af3107a4000", "buy_amount": "0x5af3107a4000",
                    "private_minimum": "0x5af3107a4000", "shield_fee_bps": "0x0",
                    "slippage_bps": 50, "pre_hook_gas_limit": 0, "post_hook_gas_limit": 0,
                    "anchors": []
                },
                "pre_hook": { "nonce": U256::ONE, "payload": B256::repeat_byte(0xb1) },
                "post_hook": null, "invalidates": null,
                "observations": SwapOrderObservations {
                    pre_hook_executed: Some(SwapObservation {
                        block: BlockNumHash::new(READ_AT - 80, B256::repeat_byte(0xb7)),
                        transaction_hash: None,
                    }),
                    ..Default::default()
                },
            }],
        });
        Some(swap)
    };
    let operation = || {
        let mut operation = account(
            8,
            0x3c,
            1,
            vec![staged_payload(0, 0xa0, Operation, Some(0xc8))],
        )?;
        operation["purpose_summary"] = serde_json::json!(format!(
            "Unshield 0.001 WETH → {} (unwrap)",
            Address::repeat_byte(0x42)
        ));
        Some(operation)
    };
    // A settled native shield, hidden as one is once its Shield event is proven.
    let native_shield = || {
        let mut shield = account(
            9,
            0x3d,
            1,
            vec![staged_payload(0, 0xa1, Operation, Some(0xc9))],
        )?;
        shield["purpose_summary"] = serde_json::json!(
            "Shield 0.01 ETH → 0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp"
        );
        shield["hidden"] = serde_json::json!(true);
        Some(shield)
    };
    [swap(), operation(), native_shield()]
        .into_iter()
        .flatten()
        .filter_map(|account| serde_json::from_value(account).ok())
        .collect()
}

/// What the refund dialog shows for the blocked Shield of a `refund:` mode. `None` for any
/// other Shield, in every other mode, and in `refund:stealth` without its staged accounts.
pub(in crate::root) fn refund_dialog_state(
    utxo_id: BlockedShieldRescueUtxoId,
) -> Option<BlockedShieldRefundDialogState> {
    let Mode::Refund(refund) = mode()? else {
        return None;
    };
    if utxo_id != REFUND_UTXO {
        return None;
    }
    let unheld = Some(Address::repeat_byte(0x4e));
    // Each reason repeats the sentence wallet-ops gives that case.
    let (reason, blocker, origin) = match refund {
        Refund::Checking => return Some(BlockedShieldRefundDialogState::Checking),
        Refund::Available => {
            return Some(BlockedShieldRefundDialogState::Available(
                BlockedShieldRescueInfo {
                    eligible: true,
                    disabled_reason: None,
                    origin_address: unheld.map(|origin| origin.to_checksum(None)),
                    public_account_uuid: Some("ui-fixture".to_owned()),
                    public_account_label: Some("Savings".to_owned()),
                },
            ));
        }
        Refund::Unknown => (
            "The Shield came from an account that isn't in this wallet.".to_owned(),
            Some(BlockedShieldRescueBlocker::OriginUnknown),
            unheld,
        ),
        Refund::Stealth => {
            // The first account Stealth accounts shows, so the dialog's action finds its row.
            let accounts = stealth_accounts().unwrap_or_default();
            let account = accounts.first()?;
            let index = account.index();
            (
                format!("The Shield came from stealth account #{index}, which isn't in Public."),
                Some(BlockedShieldRescueBlocker::OriginStealth {
                    operation: account.operation(),
                    index,
                }),
                account.address(),
            )
        }
        Refund::Inactive => (
            "The Shield came from Public account \"Savings\", which is inactive.".to_owned(),
            Some(BlockedShieldRescueBlocker::OriginInactive {
                public_account_uuid: "ui-fixture".to_owned(),
                label: Some("Savings".to_owned()),
            }),
            unheld,
        ),
        Refund::Unresolved => (
            "Couldn't read the Shield transaction. Check the connection and try again.".to_owned(),
            None,
            None,
        ),
    };
    Some(BlockedShieldRefundDialogState::Unavailable {
        info: BlockedShieldRescueInfo {
            eligible: false,
            disabled_reason: Some(reason),
            origin_address: origin.map(|origin| origin.to_checksum(None)),
            public_account_uuid: None,
            public_account_label: None,
        },
        blocker,
    })
}
