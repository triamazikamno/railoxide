//! The debug UI fixture's `walletconnect-request` mode, whose contract is in the module doc of
//! `private_swap/ui_fixture.rs`. Its one dapp request lives in memory, with no session, route
//! or record, and nothing approves, quotes or answers it.

use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::{Address, Bytes, U256, address};
use alloy::sol;
use alloy::sol_types::SolCall as _;
use railgun_ui::known_tokens_for_chain;
use serde_json::json;
use wallet_ops::vault::{
    PublicAccountMetadata, PublicAccountScope, PublicAccountSource, PublicAccountStatus,
};
use wallet_ops::{EvmFeeModel, PublicActionGasFeeQuote, parse_walletconnect_session_request};

use super::{
    DappRequestAccount, DappRequestBinding, DappSessionIdentity, WalletConnectRequestUi, WalletRoot,
};

sol! {
    interface FixtureErc20 {
        function approve(address spender, uint256 amount) external;
    }
}

const KEY: &str = "fixture:walletconnect-request";
const TRANSPORT: &str = "fixture";
/// Uniswap's Permit2, which the canned request lets spend WETH without limit.
const SPENDER: Address = address!("0x000000000022D473030F116dDEE9F6B43aC78BA3");

/// The canned request's fee: a base fee of 1 gwei and a tip of 0.1 gwei.
pub(super) const FEE_QUOTE: PublicActionGasFeeQuote = PublicActionGasFeeQuote {
    observed_fee_model: Some(EvmFeeModel::Eip1559),
    rpc_gas_price: 1_100_000_000,
    current_base_fee_per_gas: Some(1_000_000_000),
    suggested_max_fee_per_gas: 1_300_000_000,
    suggested_max_priority_fee_per_gas: 100_000_000,
};

/// Whether `request` is the fixture's canned one.
pub(super) fn is_fixture_request(request: &WalletConnectRequestUi) -> bool {
    request.key == KEY && request.session_identity.transport == TRANSPORT
}

impl WalletRoot {
    /// In the `walletconnect-request` mode, stage the canned request the first time the
    /// unlocked wallet renders, so the request dialog opens on it.
    pub(in crate::root) fn stage_ui_fixture_walletconnect_request(&mut self) {
        static STAGED: AtomicBool = AtomicBool::new(false);
        if !crate::root::private_swap::ui_fixture_walletconnect_request()
            || STAGED.swap(true, Ordering::Relaxed)
        {
            return;
        }
        let Some(request) = self
            .public_accounts
            .iter()
            .find(|account| {
                account.source != PublicAccountSource::HardwareDerived
                    && account.status == PublicAccountStatus::Active
                    && account.is_available_on_chain(1)
            })
            .and_then(fixture_request)
        else {
            tracing::warn!(
                "the debug UI fixture couldn't stage its dapp request, which needs an active \
                 software Public account on Ethereum"
            );
            return;
        };
        self.walletconnect
            .pending_requests
            .insert(KEY.to_owned(), request);
    }
}

/// An unlimited approval of Ethereum's WETH to `SPENDER` from `account`.
fn fixture_request(account: &PublicAccountMetadata) -> Option<WalletConnectRequestUi> {
    let weth = known_tokens_for_chain(1)
        .find(|token| token.symbol == "WETH")?
        .token;
    let data = Bytes::from(
        FixtureErc20::approveCall {
            spender: SPENDER,
            amount: U256::MAX,
        }
        .abi_encode(),
    );
    let params = json!([{ "from": account.address, "to": weth, "data": data }]);
    let parsed = parse_walletconnect_session_request(1, "eth_sendTransaction", &params).ok()?;
    let item = parsed.desktop_approval(
        1,
        TRANSPORT,
        "Uniswap",
        "eip155:1",
        Some(account.address),
        None,
    );
    let owning_private_wallet_uuid = match &account.scope {
        PublicAccountScope::PrivateWallet { wallet_uuid } => Some(wallet_uuid.clone()),
        PublicAccountScope::Global => None,
    };
    Some(WalletConnectRequestUi {
        key: KEY.to_owned(),
        review_token: 1,
        binding: DappRequestBinding {
            account: Some(DappRequestAccount {
                public_account_uuid: account.public_account_uuid.clone(),
                public_account_scope: account.scope.clone(),
                owning_private_wallet_uuid,
            }),
            peer_name: "Uniswap".to_owned(),
            peer_url: "https://app.uniswap.org".to_owned(),
        },
        session_identity: DappSessionIdentity {
            transport: TRANSPORT,
            id: KEY.to_owned(),
        },
        parsed,
        item,
        account_source: Some(account.source),
        request_control: None,
        rpc_reads: None,
        operation: None,
    })
}
