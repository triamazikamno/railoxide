use super::*;

#[test]
fn public_action_native_max_subtracts_estimated_gas_reserve() {
    assert_eq!(
        public_action_max_amount_after_reserve(U256::from(100_u64), U256::from(40_u64)),
        Some(U256::from(60_u64)),
    );
    assert_eq!(
        public_action_max_amount_after_reserve(U256::from(100_u64), U256::from(100_u64)),
        None,
    );
    assert_eq!(
        public_action_max_amount_after_reserve(U256::from(100_u64), U256::from(101_u64)),
        None,
    );
}

#[test]
fn public_action_max_label_notes_native_gas_estimate() {
    let native = PublicBalanceEntry {
        asset: PublicBalanceAsset {
            id: PublicAssetId::Native,
            symbol: "ETH".to_string(),
            decimals: 18,
        },
        amount: PublicBalanceAmount::Available(U256::from(1_000_000_000_000_000_000_u128)),
    };
    let token = PublicBalanceEntry {
        asset: PublicBalanceAsset {
            id: PublicAssetId::Erc20(Address::from([0x22; 20])),
            symbol: "USDC".to_string(),
            decimals: 6,
        },
        amount: PublicBalanceAmount::Available(U256::from(1_500_000_u64)),
    };

    assert_eq!(
        public_action_max_label(&native, Some(U256::from(286_838_782_386_224_461_u128)),),
        Some("0.7132 ETH after est. gas".to_string()),
    );
    assert_eq!(
        public_action_max_label(&token, None),
        Some("1.5 USDC".to_string()),
    );
    assert_eq!(public_action_max_label(&native, None), None);
}

#[test]
fn public_action_progress_steps_use_single_send_wrap_or_unwrap_step() {
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Send,
            PublicAssetId::Native,
            wallet_ops::vault::PublicAccountSource::Derived,
            false,
        ),
        vec![PublicActionProgressStep::Send],
    );
    // Wrap Max reserves gas with these steps, so a native wrap never adds Approve or Shield.
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Wrap,
            PublicAssetId::Native,
            wallet_ops::vault::PublicAccountSource::HardwareDerived,
            true,
        ),
        vec![PublicActionProgressStep::Wrap],
    );
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Unwrap,
            PublicAssetId::Erc20(Address::from([0xef; 20])),
            wallet_ops::vault::PublicAccountSource::HardwareDerived,
            false,
        ),
        vec![PublicActionProgressStep::Unwrap],
    );
}

#[test]
fn public_unwrap_gas_error_blocks_only_a_known_short_native_balance() {
    let label = |cost: U256| format!("{cost} wei");
    assert_eq!(
        public_unwrap_gas_error(Some(U256::from(99_u64)), U256::from(100_u64), label),
        Some("Unwrap needs up to 100 wei for gas".to_string()),
    );
    assert_eq!(
        public_unwrap_gas_error(Some(U256::from(100_u64)), U256::from(100_u64), label),
        None,
    );
    assert_eq!(
        public_unwrap_gas_error(None, U256::from(100_u64), label),
        None
    );
}

#[test]
fn public_action_closed_active_step_uses_pending_step() {
    let steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Wrap,
            status: PublicActionStepStatus::Error,
            tx_hash: None,
            message: Some(Arc::from("wrap failed")),
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::Pending,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];

    assert_eq!(
        public_action_closed_active_step(&steps).map(|step| step.step),
        Some(PublicActionProgressStep::Approve),
    );
    assert!(steps.iter().all(|step| step.interval.is_none()));
}

#[test]
fn public_action_closed_active_step_uses_error_without_pending() {
    let steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Wrap,
            status: PublicActionStepStatus::Done,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::Error,
            tx_hash: None,
            message: Some(Arc::from("approve failed")),
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];

    assert_eq!(
        public_action_closed_active_step(&steps).map(|step| step.step),
        Some(PublicActionProgressStep::Approve),
    );
}

#[test]
fn public_action_closed_active_step_ignores_inactive_steps() {
    let steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Wrap,
            status: PublicActionStepStatus::Done,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];

    assert!(public_action_closed_active_step(&steps).is_none());
}

#[test]
fn public_action_progress_steps_use_single_transaction_for_native_shield() {
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Shield,
            PublicAssetId::Native,
            wallet_ops::vault::PublicAccountSource::Derived,
            false,
        ),
        vec![PublicActionProgressStep::Shield],
    );
}

#[test]
fn public_action_progress_steps_skip_wrap_for_erc20_shield() {
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Shield,
            PublicAssetId::Erc20(Address::from([0xef; 20])),
            wallet_ops::vault::PublicAccountSource::Derived,
            false,
        ),
        vec![
            PublicActionProgressStep::Approve,
            PublicActionProgressStep::Shield,
        ],
    );
}

#[test]
fn hardware_public_shield_progress_steps_start_with_shield_key_authorization() {
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Shield,
            PublicAssetId::Erc20(Address::from([0xef; 20])),
            wallet_ops::vault::PublicAccountSource::HardwareDerived,
            false,
        ),
        vec![
            PublicActionProgressStep::ShieldKey,
            PublicActionProgressStep::Approve,
            PublicActionProgressStep::Shield,
        ],
    );
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Shield,
            PublicAssetId::Native,
            wallet_ops::vault::PublicAccountSource::HardwareDerived,
            false,
        ),
        vec![
            PublicActionProgressStep::ShieldKey,
            PublicActionProgressStep::Shield,
        ],
    );
    // A hardware-native payer's native shield takes the wrap fallback.
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Shield,
            PublicAssetId::Native,
            wallet_ops::vault::PublicAccountSource::HardwareDerived,
            true,
        ),
        vec![
            PublicActionProgressStep::ShieldKey,
            PublicActionProgressStep::Wrap,
            PublicActionProgressStep::Approve,
            PublicActionProgressStep::Shield,
        ],
    );
}

#[test]
fn public_action_error_summary_explains_wrap_gas_estimate() {
    assert_eq!(
        public_action_error_summary(
            PublicActionProgressStep::Wrap,
            Some("public-shield-wrap: estimate gas"),
            "ETH",
        ),
        "Could not estimate gas to wrap ETH. Check amount and gas balance.",
    );
}

#[test]
fn public_action_asset_label_uses_native_symbol() {
    assert_eq!(
        public_action_asset_label(
            wallet_ops::settings::build_effective_chain_configs(
                &wallet_ops::settings::WalletSettings::default()
            )
            .unwrap()
            .get(1),
            PublicAssetId::Native,
            None
        ),
        "ETH"
    );
}

#[test]
fn public_action_error_details_hide_duplicate_summary() {
    let summary = "Could not send publicly.";

    assert_eq!(public_action_error_details(summary, Some(summary)), None);
    assert_eq!(
        public_action_error_details(
            "The transaction expired without being included. No TRX was spent.",
            Some("the transaction expired without being included; no TRX was spent"),
        ),
        None,
    );
    assert_eq!(
        public_action_error_details(summary, Some("public-send: estimate gas")),
        Some("public-send: estimate gas".to_string()),
    );
}

#[test]
fn public_action_error_copy_value_includes_context_and_details() {
    assert_eq!(
        public_action_error_copy_value(
            PublicActionProgressStep::Wrap,
            "ETH",
            "Could not estimate gas to wrap ETH.",
            Some("public-shield-wrap: estimate gas: insufficient funds"),
        ),
        "Step: Wrap\nAsset: ETH\nSummary: Could not estimate gas to wrap ETH.\nDetails: public-shield-wrap: estimate gas: insufficient funds",
    );
}
