use super::*;
use crate::root::actions::*;
use gpui::{
    AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Render, Styled,
    TestAppContext, WeakEntity, Window, div,
};
use gpui_component::Root;
use wallet_ops::settings::{WalletSettings, build_effective_chain_configs};

struct PublicListWindow(Entity<WalletRoot>);
impl Render for PublicListWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_3()
            .pl(gpui::px(if window.viewport_size().width < gpui::px(900.) {
                60.
            } else {
                232.
            }))
            .child(
                self.0
                    .read(cx)
                    .render_public_wallet_body(&self.0, window, cx),
            )
    }
}

pub(in crate::root) fn fixture_root(
    path: &std::path::Path,
    runtime: &tokio::runtime::Runtime,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Entity<WalletRoot> {
    const PASSWORD: &str = "public list test password";
    let vault = Arc::new(DesktopVaultStore::open(path.to_path_buf()).unwrap());
    vault
        .create_vault_with_params(PASSWORD, wallet_ops::vault::KdfParams::new(1024, 1, 1))
        .unwrap();
    let metadata = vault
        .new_wallet_metadata(
            PASSWORD,
            "preview",
            0,
            WalletSource::Imported,
            "Public list test",
        )
        .unwrap();
    vault.import_wallet_mnemonic_with_metadata(PASSWORD, "preview", 0, "english", "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", &metadata).unwrap();
    let view_session = Arc::new(vault.load_view_session(PASSWORD, "preview").unwrap());
    let http = wallet_ops::build_http_client(None).unwrap();
    let monitor_state = broadcaster_monitor::shared();
    let (events, event_rx) = broadcaster_monitor::event_channel(1);
    let cache = Arc::new(TokenAnchorRateCache::new());
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    let tokens = EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let refresh = wallet_ops::spawn_token_anchor_refresh_worker(
        runtime.handle(),
        cache.clone(),
        Vec::new(),
        std::iter::empty().collect(),
        tokens.clone(),
        http.clone(),
    );
    let monitor = cx.new(|cx| {
        broadcaster_monitor_gpui::BroadcasterMonitorPane::new(
            monitor_state.clone(),
            event_rx.clone(),
            &[1],
            1,
            Vec::new(),
            Arc::new(|_, _| None),
            window,
            cx,
        )
    });
    let logs = cx.new(|cx| ui::logs::LogsPane::new(ui::logs::LogStore::new(1), window, cx));
    let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
    let mut chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .unwrap()
        .clone();
    // Exercise the dialog without contacting real RPC endpoints from this UI fixture.
    chain.rpc_route = wallet_ops::RpcChainRoute::new(1, Vec::<reqwest::Url>::new());
    let chains = std::iter::once(chain).collect();
    let poi = PoiReadSource::PoiProxy {
        rpc_url: reqwest::Url::parse("http://127.0.0.1:1").unwrap().into(),
    };
    cx.new(|cx| {
        let mut root = WalletRoot::new(
            WalletStartupReady {
                http,
                waku_config: WakuMonitorConfig::default(),
                monitor_event_tx: events,
                vault_store: vault,
                chain_ids: vec![1],
                initial_chain_id: 1,
                ui_state: WalletUiState::default(),
                effective_chain_configs: chains,
                effective_token_registry: tokens,
                public_balance_refresh_interval: Duration::from_mins(1),
                auto_lock_timeout: None,
                public_broadcaster_policy: BroadcasterFeePolicy::default(),
                public_broadcaster_response_timeout: Duration::from_secs(30),
                public_broadcaster_republish_interval: Duration::from_secs(5),
                default_allow_suspicious_broadcasters: false,
                mimic_railway_shields_by_default: false,
                poi_read_source: poi,
            },
            WalletRootDependencies {
                options: WalletAppOptions {
                    db_path: path.to_path_buf(),
                },
                public_transaction_tracker: wallet_ops::PublicTransactionTracker::default(),
                runtime: runtime.handle().clone(),
                monitor_state,
                monitor_event_rx: event_rx,
                monitor,
                public_broadcaster_anchor_cache: cache,
                public_broadcaster_anchor_refresh: refresh,
                logs,
                startup_root: &WeakEntity::new_invalid(),
                maintenance_controller: &maintenance,
            },
            window,
            cx,
        );
        root.focus_vault_input_on_render = false;
        root.vault_state = VaultState::ViewUnlocked;
        root.view_session = Some(view_session);
        root.wallet_metadata = vec![metadata];
        root.selected_wallet_id = Some(Arc::from("preview"));
        root.public_accounts = (0..15)
            .map(|index| PublicAccountMetadata {
                public_account_uuid: format!("account-{index}"),
                address: Address::repeat_byte(index),
                label: if index == 13 {
                    None
                } else {
                    Some(format!("Account {index:02}"))
                },
                source: PublicAccountSource::Derived,
                scope: PublicAccountScope::PrivateWallet {
                    wallet_uuid: "preview".into(),
                },
                derivation_index: Some(u32::from(index)),
                hardware_descriptor: None,
                status: if index == 14 {
                    PublicAccountStatus::Inactive
                } else {
                    PublicAccountStatus::Active
                },
                display_order: u32::from(index),
            })
            .collect();
        root.public_balance_snapshot = Some(Arc::new(PublicBalanceSnapshot {
            chain_id: 1,
            refreshed_at: SystemTime::now(),
            accounts: root
                .public_accounts
                .iter()
                .take(14)
                .map(|account| {
                    let amount = if account.display_order == 12 {
                        U256::ZERO
                    } else {
                        uint!(1_000_000_000_000_000_000_U256)
                    };
                    PublicAccountBalance {
                        account: account.clone(),
                        observed_at: None,
                        observed_block: None,
                        balances: [
                            PublicAssetId::Native,
                            PublicAssetId::Erc20(Address::repeat_byte(42)),
                            PublicAssetId::Erc20(Address::repeat_byte(43)),
                            PublicAssetId::Erc20(Address::repeat_byte(44)),
                        ]
                        .into_iter()
                        .map(|id| PublicBalanceEntry {
                            asset: PublicBalanceAsset {
                                id,
                                symbol: if id == PublicAssetId::Native {
                                    "ETH"
                                } else {
                                    "TOKEN"
                                }
                                .into(),
                                decimals: 18,
                            },
                            amount: PublicBalanceAmount::Available(amount),
                        })
                        .collect(),
                    }
                })
                .collect(),
        }));
        root.reconcile_public_account_selection();
        root
    })
}

/// Chains 10 and 20 without Railgun, each with an unreachable RPC so a balance scope exists.
/// Started reads never complete: the fixture's runtime is not driven.
fn use_public_only_test_chains(root: &mut WalletRoot) {
    let template = root.effective_chain_configs.get(1).unwrap().clone();
    root.effective_chain_configs = [10, 20]
        .into_iter()
        .map(|chain_id| {
            let mut chain = template.clone();
            chain.chain_id = chain_id;
            chain.railgun = None;
            chain.rpc_route = wallet_ops::RpcChainRoute::new(
                chain_id,
                vec![reqwest::Url::parse("http://127.0.0.1:1").unwrap()],
            );
            chain
        })
        .collect();
    root.selected_network = crate::root::SelectedNetwork::Evm(10);
    root.last_evm_chain = 10;
    root.public_balance_snapshot = None;
    root.publish_gateway_desktop_state();
}

fn observed_native_balance(account: &PublicAccountMetadata) -> PublicAccountBalance {
    PublicAccountBalance {
        account: account.clone(),
        observed_at: Some(std::time::Instant::now()),
        observed_block: Some(10),
        balances: vec![PublicBalanceEntry {
            asset: PublicBalanceAsset {
                id: PublicAssetId::Native,
                symbol: "ETH".into(),
                decimals: 18,
            },
            amount: PublicBalanceAmount::Available(uint!(1_000_000_000_000_000_000_U256)),
        }],
    }
}

const TRON_TEST_PASSWORD: &str = "public list test password";
const TRON_TEST_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

const TRX_SUN: u64 = 1_000_000;

fn clock_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A Tron chain for Send on which every account exists and a reference block is the clock's.
/// With `hang`, a send stops at its reference block read, before anything is signed. Its
/// solidified blocks include every broadcast transaction, or with `includes` false, pass their
/// expiration without them. While `hold` is set, the solidified head isn't read yet. With
/// `hold_next_preparation`, the next fee parameters read waits for `release_preparation`. Token
/// transfers need no energy, except to [`TRON_TOKENLESS_RECEIVER`].
struct TronSendChain {
    hang: bool,
    reached_signing: std::sync::atomic::AtomicBool,
    hold: std::sync::atomic::AtomicBool,
    hold_next_preparation: std::sync::atomic::AtomicBool,
    release_preparation: std::sync::atomic::AtomicBool,
    broadcast: Result<wallet_ops::tron::http_api::TronBroadcast, wallet_ops::RpcBrokerError>,
    includes: bool,
    broadcasts: std::sync::Mutex<Vec<alloy::primitives::B256>>,
}

impl TronSendChain {
    fn new(
        broadcast: Result<wallet_ops::tron::http_api::TronBroadcast, wallet_ops::RpcBrokerError>,
        includes: bool,
    ) -> Self {
        Self {
            hang: false,
            reached_signing: std::sync::atomic::AtomicBool::new(false),
            hold: std::sync::atomic::AtomicBool::new(false),
            hold_next_preparation: std::sync::atomic::AtomicBool::new(false),
            release_preparation: std::sync::atomic::AtomicBool::new(false),
            broadcast,
            includes,
            broadcasts: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn broadcasts(&self) -> Vec<alloy::primitives::B256> {
        self.broadcasts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl wallet_ops::tron::send::TronSendReads for TronSendChain {
    async fn account_exists(
        &self,
        _account: alloy::primitives::Address,
    ) -> Result<bool, wallet_ops::RpcBrokerError> {
        Ok(true)
    }

    async fn account_resources(
        &self,
        _account: alloy::primitives::Address,
    ) -> Result<wallet_ops::tron::fees::AccountResources, wallet_ops::RpcBrokerError> {
        Ok(wallet_ops::tron::fees::AccountResources::default())
    }

    async fn chain_fee_parameters(
        &self,
    ) -> Result<wallet_ops::tron::fees::ChainFeeParameters, wallet_ops::RpcBrokerError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.hold_next_preparation.swap(false, SeqCst) {
            while !self.release_preparation.load(SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        Ok(wallet_ops::tron::fees::ChainFeeParameters {
            bandwidth_price: 1_000,
            energy_price: 100,
            create_account_fee: TRX_SUN / 10,
            create_account_system_fee: TRX_SUN,
            create_account_bandwidth_rate: 1,
        })
    }

    async fn energy_estimate(
        &self,
        _sender: alloy::primitives::Address,
        _contract: alloy::primitives::Address,
        data: alloy::primitives::Bytes,
    ) -> Result<u64, wallet_ops::RpcBrokerError> {
        // `transfer(address,uint256)`: the recipient ends the first argument word.
        let recipient = data.get(16..36).map(alloy::primitives::Address::from_slice);
        Ok(if recipient == Some(TRON_TOKENLESS_RECEIVER) {
            130_285
        } else {
            0
        })
    }

    async fn latest_block(
        &self,
    ) -> Result<wallet_ops::tron::transaction::ReferenceBlock, wallet_ops::RpcBrokerError> {
        self.reached_signing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if self.hang {
            std::future::pending::<()>().await;
        }
        Ok(wallet_ops::tron::transaction::ReferenceBlock {
            number: 100,
            hash: alloy::primitives::B256::repeat_byte(1),
            timestamp_ms: clock_ms(),
        })
    }

    async fn broadcast(
        &self,
        transaction: &wallet_ops::tron::transaction::SignedTransaction,
    ) -> Result<wallet_ops::tron::http_api::TronBroadcast, wallet_ops::RpcBrokerError> {
        self.broadcasts.lock().unwrap().push(transaction.txid());
        self.broadcast.clone()
    }

    fn now_ms(&self) -> u64 {
        clock_ms()
    }
}

impl TronSendChain {
    fn solidified_block(&self, number: u64) -> wallet_ops::tron::confirmation::TronBlock {
        let now_ms = clock_ms();
        wallet_ops::tron::confirmation::TronBlock {
            number,
            hash: alloy::primitives::B256::repeat_byte(2),
            timestamp_ms: if self.includes {
                now_ms
            } else {
                now_ms + 2 * wallet_ops::tron::transaction::EXPIRATION_WINDOW_MS
            },
            transactions: if self.includes {
                self.broadcasts()
            } else {
                Vec::new()
            },
        }
    }
}

#[async_trait::async_trait]
impl wallet_ops::tron::confirmation::TronBlockReads for TronSendChain {
    async fn solidified_head(
        &self,
    ) -> Result<wallet_ops::tron::confirmation::TronBlock, wallet_ops::RpcBrokerError> {
        while self.hold.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(self.solidified_block(101))
    }

    async fn block(
        &self,
        number: u64,
    ) -> Result<wallet_ops::tron::confirmation::TronBlock, wallet_ops::RpcBrokerError> {
        Ok(self.solidified_block(number))
    }

    async fn transaction_infos(
        &self,
        _number: u64,
    ) -> Result<Vec<wallet_ops::tron::http_api::TronTransactionInfo>, wallet_ops::RpcBrokerError>
    {
        Ok(self
            .broadcasts()
            .into_iter()
            .map(|id| wallet_ops::tron::http_api::TronTransactionInfo {
                id,
                succeeded: true,
                logs: Vec::new(),
                fee_sun: 0,
            })
            .collect())
    }
}

/// A wallet with an imported Tron account holding 10 TRX, on Tron, with that account's Send
/// form open over `chain`. Tron isn't activated, so nothing reads its route. Returns the intent
/// that approves the review of a 1 TRX send.
fn open_tron_send(
    root: &Entity<WalletRoot>,
    cx: &mut gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
    chain: &Arc<TronSendChain>,
) -> crate::root::public_account::TronSendIntent {
    use wallet_ops::{PublicBalanceAmount, PublicBalanceAsset, PublicBalanceEntry};
    let (uuid, sender, operation) = root.update_in(cx, |root, window, cx| {
        let account = if let Some(account) = root.tron_public.accounts.first() {
            account.clone()
        } else {
            let store = root.vault_store.clone().unwrap();
            let session = root.view_session.clone().unwrap();
            let account = store
                .import_tron_account(TRON_TEST_PASSWORD, &session, TRON_TEST_KEY, None, false)
                .unwrap();
            root.reload_public_accounts(window, cx);
            account
        };
        root.selected_network = crate::root::SelectedNetwork::Tron;
        let listed = root
            .listed_account_for_uuid(Some(&account.tron_account_uuid))
            .unwrap()
            .clone();
        root.tron_public.snapshot = Some(Arc::new(wallet_ops::PublicBalanceSnapshot {
            chain_id: wallet_ops::tron::TRON_CHAIN_ID,
            refreshed_at: std::time::SystemTime::now(),
            accounts: vec![wallet_ops::PublicAccountBalance {
                account: listed,
                balances: vec![PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Native,
                        symbol: "TRX".to_owned(),
                        decimals: 6,
                    },
                    amount: PublicBalanceAmount::Available(U256::from(10 * TRX_SUN)),
                }],
                observed_at: None,
                observed_block: None,
            }],
        }));
        root.tron_public.send.backend = Some(chain.clone());
        let uuid: Arc<str> = Arc::from(account.tron_account_uuid.as_str());
        root.open_tron_send_dialog(Arc::clone(&uuid), PublicAssetId::Native, window, cx);
        let operation = root
            .tron_public
            .send
            .form
            .as_ref()
            .unwrap()
            .operation
            .clone();
        (uuid, account.address, operation)
    });
    cx.run_until_parked();
    let review = runtime
        .block_on(wallet_ops::tron::send::prepare_tron_send(
            chain.as_ref(),
            &wallet_ops::tron::send::TronSendDraft {
                sender,
                receiver: alloy::primitives::Address::repeat_byte(0x22),
                asset: wallet_ops::tron::send::TronSendAsset::Trx,
                amount: wallet_ops::tron::send::TronSendAmount::Exact(U256::from(TRX_SUN)),
                trx_balance: 10 * TRX_SUN,
            },
        ))
        .unwrap();
    crate::root::public_account::TronSendIntent {
        account_uuid: uuid,
        account_label: "Tron".to_owned(),
        source: wallet_ops::vault::TronAccountSource::Imported,
        symbol: "TRX".to_owned(),
        decimals: 6,
        icon: None,
        review,
        operation,
    }
}

fn approve_tron_send(
    root: &Entity<WalletRoot>,
    cx: &mut gpui::VisualTestContext,
    intent: crate::root::public_account::TronSendIntent,
) {
    root.update_in(cx, |root, window, cx| {
        root.submit_tron_send_authorized(
            intent,
            wallet_ops::DesktopPrivateSpendAuthorization::VaultPassword(zeroize::Zeroizing::new(
                TRON_TEST_PASSWORD.to_owned(),
            )),
            window,
            cx,
        );
        assert!(root.public_form.sending);
    });
}

fn drive_until(
    cx: &mut gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
    mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done(cx) {
        assert!(std::time::Instant::now() < deadline, "timed out");
        runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(10)));
        cx.run_until_parked();
    }
}

const TRON_USDT: alloy::primitives::Address =
    alloy::primitives::address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c");
/// A receiver that doesn't hold the token, so a transfer to it needs more energy.
const TRON_TOKENLESS_RECEIVER: alloy::primitives::Address =
    alloy::primitives::Address::repeat_byte(0x33);

fn set_tron_test_receiver(
    root: &mut WalletRoot,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    let receiver = wallet_ops::tron::address::encode_tron_address(
        alloy::primitives::Address::repeat_byte(0x22),
    );
    root.set_tron_receiver(&receiver, window, cx);
}

/// Lets the open Tron Send form's pending preparation start.
fn start_tron_send_preparation(cx: &gpui::VisualTestContext) {
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
}

mod balances_and_shortcuts;
mod chain_selection;
mod public_list;
mod tron_accounts;
mod tron_send;
mod tron_send_review;
mod tron_send_routes;
