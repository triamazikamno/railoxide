use super::*;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use gpui::{Render, TestAppContext, VisualTestContext};
use gpui_component::WindowExt as _;
use serde_json::{Value, json};
use wallet_ops::vault::{WalletSource, WalletViewBundle, wallet_keys_from_mnemonic};
use wallet_ops::{RpcChainRoute, RpcEndpointRoles, RpcTransport};

use crate::root::device_auth::DeviceAuthPassword;
use crate::root::vault::WalletOption;

const PASSWORD: &str = "public list test password";
const MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

struct DialogWindow;

impl Render for DialogWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn fixture<'a>(
    cx: &'a mut TestAppContext,
    directory: &std::path::Path,
    runtime: &tokio::runtime::Runtime,
) -> (Entity<WalletRoot>, &'a mut VisualTestContext) {
    // Authorization finishes on a blocking thread.
    cx.executor().allow_parking();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        root = Some(crate::root::tests::public_accounts::fixture_root(
            directory, runtime, window, cx,
        ));
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));
    (root.unwrap(), cx)
}

fn account_count(root: &WalletRoot) -> usize {
    root.vault_store
        .as_ref()
        .unwrap()
        .list_all_public_accounts(root.view_session.as_ref().unwrap())
        .unwrap()
        .len()
}

fn open_derive(root: &Entity<WalletRoot>, cx: &mut VisualTestContext) -> Weak<Cell<bool>> {
    let lease = cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.open_public_account_dialog(PublicAccountDialogKind::Derive, window, cx)
        })
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    lease
}

fn start_authorization(root: &Entity<WalletRoot>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.public_form
                .add_password_input
                .update(cx, |input, cx| input.set_value(PASSWORD, window, cx));
            root.add_public_derived_account_from_input(window, cx);
        });
    });
}

fn wait_for_authorization(
    root: &Entity<WalletRoot>,
    runtime: &tokio::runtime::Runtime,
    cx: &mut VisualTestContext,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while root.read_with(cx, |root, _| root.public_form.derive_preview.authorizing) {
        assert!(Instant::now() < deadline, "preview authorization timed out");
        runtime.block_on(tokio::time::sleep(Duration::from_millis(10)));
        cx.run_until_parked();
    }
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

fn authorize(
    root: &Entity<WalletRoot>,
    runtime: &tokio::runtime::Runtime,
    cx: &mut VisualTestContext,
) {
    start_authorization(root, cx);
    wait_for_authorization(root, runtime, cx);
}

#[gpui::test]
fn derive_dialog_stays_in_authorize_on_failure(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);

    let lease = open_derive(&root, cx);
    assert!(cx.debug_bounds("public-derive-authorize").is_some());
    for outcome in [
        DeviceAuthPassword::Cancelled,
        DeviceAuthPassword::Failed(Arc::from("device authentication failed")),
    ] {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.device_auth_in_progress = true;
                let generation = root.active_wallet_generation;
                root.finish_public_account_device_auth(
                    PublicAccountDialogKind::Derive,
                    &lease,
                    generation,
                    outcome,
                    window,
                    cx,
                );
                let state = &root.public_form.derive_preview;
                assert!(state.preview.is_none() && !state.authorizing);
            });
        });
    }
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.error.clone())
            .as_deref(),
        Some("device authentication failed")
    );

    // A passphrase context derives only through its protected seed session.
    let context = "70617373706872617373652d636f6e74";
    root.update(cx, |root, _| {
        let store = root.vault_store.clone().unwrap();
        let metadata = store
            .new_passphrase_context_metadata(PASSWORD, "preview", context, 3, "Passphrase")
            .unwrap();
        let wallet = wallet_keys_from_mnemonic(MNEMONIC, "TREZOR", 3).unwrap();
        store
            .store_passphrase_context_with_view_bundle(
                PASSWORD,
                "preview",
                context,
                &WalletViewBundle::from_wallet_keys(3, &wallet),
                &metadata,
            )
            .unwrap();
        root.view_session = Some(Arc::new(
            store.load_view_session(PASSWORD, context).unwrap(),
        ));
        root.protected_software_seed_session = None;
    });
    open_derive(&root, cx);
    authorize(&root, &runtime, cx);
    root.read_with(cx, |root, _| {
        assert!(root.public_form.derive_preview.preview.is_none());
        assert_eq!(
            root.public_form.error,
            Some(vault_error_message(
                &VaultError::SoftwareSeedSessionRequired
            ))
        );
    });
    assert!(cx.debug_bounds("public-derive-authorize").is_some());
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn derive_preview_pages_selects_and_adds_with_enter(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);
    let before = root.read_with(cx, |root, _| account_count(root));

    open_derive(&root, cx);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.public_form
                .add_password_input
                .update(cx, |input, cx| input.set_value(PASSWORD, window, cx));
        });
    });
    cx.simulate_keystrokes("enter");
    wait_for_authorization(&root, &runtime, cx);
    assert!(cx.debug_bounds("public-derive-preview").is_some());
    root.read_with(cx, |root, _| {
        let preview = root.public_form.derive_preview.preview.as_ref().unwrap();
        assert!(preview.selected.is_empty());
        // The fixture's accounts use indexes 0 to 13, and 14 is inactive.
        let statuses = root.derived_index_statuses();
        assert_eq!(statuses.get(&13), Some(&PublicAccountStatus::Active));
        assert_eq!(statuses.get(&14), Some(&PublicAccountStatus::Inactive));
        assert_eq!(statuses.get(&15), None);
    });
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        root.read_with(cx, |root, _| account_count(root)),
        before,
        "Enter added with nothing selected"
    );

    // Clicking an available row toggles it, and clicking an added row does nothing.
    for (row, expected) in [
        ("public-derive-row-15", BTreeSet::from([15])),
        ("public-derive-row-13", BTreeSet::from([15])),
        ("public-derive-row-15", BTreeSet::new()),
    ] {
        let bounds = cx.debug_bounds(row).expect(row);
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            root.read_with(cx, |root, _| preview(root).selected.clone()),
            expected,
            "{row}"
        );
    }

    // Select all on page takes only the page's Available indexes, and clearing removes them.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.select_derive_preview_page(true, window, cx);
            assert_eq!(preview(root).selected, BTreeSet::from([15, 16, 17, 18, 19]));
            root.select_derive_preview_page(false, window, cx);
            assert!(preview(root).selected.is_empty());
        });
    });

    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.toggle_derive_preview_index(15, true, window, cx);
            root.toggle_derive_preview_index(16, true, window, cx);
            root.show_derive_preview_page(20, window, cx);
            root.toggle_derive_preview_index(23, true, window, cx);
            for entry in ["2147483648", "next", "45"] {
                root.public_form
                    .derive_preview
                    .go_to_input
                    .update(cx, |input, cx| input.set_value(entry, window, cx));
                root.go_to_derive_preview_index(window, cx);
                let preview = root.public_form.derive_preview.preview.as_ref().unwrap();
                if entry == "45" {
                    assert_eq!(preview.page_start, 40);
                    assert!(preview.go_to_error.is_none());
                } else {
                    assert_eq!(preview.page_start, 20, "{entry} changed the page");
                    assert!(preview.go_to_error.is_some());
                }
            }
            let preview = root.public_form.derive_preview.preview.as_ref().unwrap();
            assert_eq!(preview.selected, BTreeSet::from([15, 16, 23]));
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    root.read_with(cx, |root, _| {
        assert_eq!(
            account_count(root),
            before + 3,
            "{:?}",
            root.public_form.error
        );
        let preview = root.public_form.derive_preview.preview.as_ref().unwrap();
        assert!(preview.selected.is_empty());
        let added = preview.last_add.as_ref().unwrap();
        assert_eq!((added.added.len(), added.skipped.len()), (3, 0));
        let statuses = root.derived_index_statuses();
        for index in [15, 16, 23] {
            assert_eq!(statuses.get(&index), Some(&PublicAccountStatus::Active));
        }
    });
    cx.update(|window, cx| assert!(window.has_active_dialog(cx)));

    // A new preview selects the index after the highest one used.
    open_derive(&root, cx);
    authorize(&root, &runtime, cx);
    root.read_with(cx, |root, _| {
        let preview = root.public_form.derive_preview.preview.as_ref().unwrap();
        assert_eq!(preview.selected, BTreeSet::from([24]));
        assert_eq!(preview.page_start, 20);
    });

    // Once the last index is used nothing is preselected, and lower indexes stay selectable.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let store = root.vault_store.clone().unwrap();
            let view = root.view_session.clone().unwrap();
            let preview = root.public_form.derive_preview.preview.as_ref().unwrap();
            store
                .add_previewed_public_accounts(&view, &preview.keys, &[INDEX_LIMIT - 1])
                .unwrap();
            root.reload_public_accounts(window, cx);
        });
    });
    open_derive(&root, cx);
    authorize(&root, &runtime, cx);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let preview = root.public_form.derive_preview.preview.as_ref().unwrap();
            assert!(preview.selected.is_empty());
            assert_eq!(preview.page_start, 0);
            root.toggle_derive_preview_index(17, true, window, cx);
            let preview = root.public_form.derive_preview.preview.as_ref().unwrap();
            assert_eq!(preview.selected, BTreeSet::from([17]));
        });
    });
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn derive_preview_ends_on_wallet_change_and_ignores_late_authorization(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);

    open_derive(&root, cx);
    authorize(&root, &runtime, cx);
    assert!(root.read_with(cx, |root, _| {
        root.public_form.derive_preview.preview.is_some()
    }));
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            // What `select_wallet` does before installing the next wallet.
            window.close_all_dialogs(cx);
            root.advance_active_wallet_generation();
            root.reset_wallet_scoped_state(cx);
            assert!(root.public_form.derive_preview.preview.is_none());
        });
    });

    let late = root.read_with(cx, |root, _| {
        root.vault_store
            .as_ref()
            .unwrap()
            .derived_public_account_preview(PASSWORD, root.view_session.as_ref().unwrap(), None)
            .unwrap()
    });
    open_derive(&root, cx);
    start_authorization(&root, cx);
    let (authorization, generation) = root.read_with(cx, |root, _| {
        (
            root.public_form.derive_preview.authorization,
            root.active_wallet_generation,
        )
    });
    let lease = open_derive(&root, cx);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.finish_derive_preview_authorization(
                authorization,
                generation,
                &lease,
                Ok(Ok(late)),
                window,
                cx,
            );
            let state = &root.public_form.derive_preview;
            assert!(state.preview.is_none() && !state.authorizing);
        });
    });
    cx.update(|window, _| window.remove_window());
}

#[test]
fn balance_check_summary_route_and_warning_follow_network_mode() {
    let mut chain = wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(1)
    .cloned()
    .unwrap();
    // An endpoint marked Direct that serves Sensitive reads doesn't change the Tor route text.
    chain.rpc_route = RpcChainRoute::new_with_transports(
        1,
        vec![
            (
                reqwest::Url::parse("http://127.0.0.1:1").unwrap().into(),
                RpcTransport::Wallet,
                RpcEndpointRoles::default(),
            ),
            (
                reqwest::Url::parse("http://127.0.0.1:2").unwrap().into(),
                RpcTransport::Direct,
                RpcEndpointRoles::default(),
            ),
        ],
    );
    let few = BTreeSet::from([23, 1, 5]);
    let (rows, warning) = balance_check_summary(&chain, WalletNetworkMode::Tor, &few);
    assert_eq!(rows[1].1, "1, 5, 23 (3 addresses)");
    assert!(rows[3].1.contains("own Tor route"));
    assert!(!rows[3].1.contains("IP") && !rows[3].1.contains("Direct"));
    assert!(warning.contains("timing"));
    let (rows, _) = balance_check_summary(&chain, WalletNetworkMode::Tor, &BTreeSet::from([7]));
    assert_eq!(rows[1].1, "7 (1 address)");
    let many: BTreeSet<u32> = (1..=12).collect();
    let (rows, _) = balance_check_summary(&chain, WalletNetworkMode::Tor, &many);
    assert_eq!(
        rows[1].1,
        "1, 2, 3, 4, 5, 6, 7, 8 and 4 more (12 addresses)"
    );
    for mode in [WalletNetworkMode::Proxy, WalletNetworkMode::Direct] {
        let (rows, warning) = balance_check_summary(&chain, mode, &few);
        assert!(rows[3].1.contains("aren't isolated"), "{mode:?}");
        assert!(warning.contains("to each other and to you"), "{mode:?}");
    }
}

type RpcCalls = Arc<Mutex<Vec<(String, Value)>>>;

/// A JSON-RPC endpoint for `chain_id` that records every call. Balance reads return zero,
/// except for the `failing` address, whose read fails.
fn spawn_rpc_mock(chain_id: u64, failing: Arc<Mutex<Option<Address>>>) -> (reqwest::Url, RpcCalls) {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let calls = RpcCalls::default();
    let recorded = Arc::clone(&calls);
    let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
    block.header.inner.number = 10;
    block.header.hash = alloy::primitives::B256::repeat_byte(10);
    let block = serde_json::to_value(block).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                return;
            };
            let (calls, failing, block) =
                (Arc::clone(&recorded), Arc::clone(&failing), block.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let method = request["method"].as_str().unwrap_or_default().to_owned();
                let params = request["params"].clone();
                calls.lock().unwrap().push((method.clone(), params.clone()));
                let mut response = match method.as_str() {
                    "eth_chainId" => json!({ "result": format!("{chain_id:#x}") }),
                    "eth_getBlockByNumber" => json!({ "result": block }),
                    "eth_getBalance"
                        if params[0].as_str().and_then(|address| address.parse().ok())
                            == *failing.lock().unwrap() =>
                    {
                        json!({ "error": { "code": -32005, "message": "limit exceeded" } })
                    }
                    "eth_getBalance" => json!({ "result": "0x0" }),
                    _ => json!({ "error": { "code": -32601, "message": "method not found" } }),
                };
                response["jsonrpc"] = json!("2.0");
                response["id"] = request["id"].clone();
                let response = response.to_string();
                let _ = write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
            });
        }
    });
    (url, calls)
}

/// The addresses whose balances `calls` read, in order.
fn balance_reads(calls: &RpcCalls) -> Vec<Address> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| method == "eth_getBalance")
        .map(|(_, params)| params[0].as_str().unwrap().parse().unwrap())
        .collect()
}

/// Serves each chain from its endpoint over Direct-mode wallet HTTP.
fn use_balance_check_chains(
    root: &Entity<WalletRoot>,
    cx: &mut VisualTestContext,
    chains: &[(u64, reqwest::Url)],
) {
    root.update(cx, |root, _| {
        root.http = wallet_ops::HttpContext::with_rpc_client_for_tests(
            reqwest::Client::new(),
            WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(|| {
            Ok(wallet_ops::OperationHttpClient::for_tests(
                reqwest::Client::new(),
                wallet_ops::OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
            ))
        });
        let template = root.effective_chain_configs.get(1).unwrap().clone();
        root.effective_chain_configs = chains
            .iter()
            .map(|(chain_id, endpoint)| {
                let mut chain = template.clone();
                chain.chain_id = *chain_id;
                chain.rpc_route = RpcChainRoute::new(*chain_id, vec![endpoint.clone()]);
                chain
            })
            .collect();
    });
}

fn preview(root: &WalletRoot) -> &DerivePreview {
    root.public_form.derive_preview.preview.as_ref().unwrap()
}

/// Drives the runtime and the test clock, which covers the waits between addresses, until
/// `done`.
fn drive_until(
    root: &Entity<WalletRoot>,
    runtime: &tokio::runtime::Runtime,
    cx: &VisualTestContext,
    mut done: impl FnMut(&WalletRoot) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if root.read_with(cx, |root, _| done(root)) {
            return;
        }
        assert!(Instant::now() < deadline, "the balance check timed out");
        runtime.block_on(tokio::time::sleep(Duration::from_millis(5)));
        cx.executor().advance_clock(Duration::from_secs(3));
    }
}

/// Gives anything wrongly started time to send.
fn drive_a_while(
    root: &Entity<WalletRoot>,
    runtime: &tokio::runtime::Runtime,
    cx: &VisualTestContext,
) {
    let mut rounds = 0;
    drive_until(root, runtime, cx, |_| {
        rounds += 1;
        rounds > 20
    });
}

#[gpui::test]
fn derive_balance_check_sends_only_selected_indexes_on_confirm(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);
    let failing = Arc::new(Mutex::new(None));
    let (selected, selected_calls) = spawn_rpc_mock(1, Arc::clone(&failing));
    let (other, other_calls) = spawn_rpc_mock(10, Arc::default());
    use_balance_check_chains(&root, cx, &[(1, selected), (10, other)]);
    open_derive(&root, cx);
    authorize(&root, &runtime, cx);
    let (page, next_page) = root.read_with(cx, |root, _| {
        let keys = &preview(root).keys;
        (keys.page(0).unwrap(), keys.page(20).unwrap())
    });
    let derived: HashSet<Address> = page.iter().chain(&next_page).map(|row| row.1).collect();
    let page_reads = |calls: &RpcCalls| {
        let mut reads = balance_reads(calls);
        reads.retain(|address| derived.contains(address));
        reads.sort();
        reads
    };
    // The fixture's accounts use indexes 0 to 13 and nothing is preselected, so the check
    // covers exactly 15 and 16 on page 0 and 23 on page 20.
    let selected_indexes = [15, 16, 23];
    let address = |index: u32| {
        page.iter()
            .chain(&next_page)
            .find(|row| row.0 == index)
            .unwrap()
            .1
    };
    *failing.lock().unwrap() = Some(address(16));

    // Paging, changing chain, selecting and a cancelled summary send nothing.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            assert!(preview(root).selected.is_empty());
            root.toggle_derive_preview_index(15, true, window, cx);
            root.toggle_derive_preview_index(16, true, window, cx);
            root.show_derive_preview_page(20, window, cx);
            root.set_derive_check_chain(10, window, cx);
            root.toggle_derive_preview_index(23, true, window, cx);
            root.set_derive_check_chain(1, window, cx);
            root.show_derive_preview_page(0, window, cx);
            root.review_derive_balance_check(true, window, cx);
            root.review_derive_balance_check(false, window, cx);
        });
    });
    drive_a_while(&root, &runtime, cx);
    assert!(page_reads(&selected_calls).is_empty() && page_reads(&other_calls).is_empty());

    // Enter confirms the open summary. Selection edits during the run don't change it.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.review_derive_balance_check(true, window, cx);
        });
        window.draw(cx).clear(cx);
    });
    assert!(cx.debug_bounds("public-derive-check-summary").is_some());
    cx.simulate_keystrokes("enter");
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            assert!(preview(root).checking.is_some());
            root.toggle_derive_preview_index(17, true, window, cx);
            root.toggle_derive_preview_index(23, false, window, cx);
        });
    });
    drive_until(&root, &runtime, cx, |root| preview(root).checking.is_none());
    // Exactly the selected addresses, once each, so no Added index's address is sent.
    let mut expected: Vec<Address> = selected_indexes.into_iter().map(address).collect();
    expected.sort();
    assert_eq!(page_reads(&selected_calls), expected);
    assert_eq!(page_reads(&other_calls), [] as [Address; 0]);
    let check = |root: &WalletRoot, index: u32| preview(root).results.get(&(1, index)).copied();
    root.read_with(cx, |root, _| {
        assert_eq!(preview(root).results.len(), selected_indexes.len());
        assert_eq!(check(root, 15), Some(BalanceCheck::Done(U256::ZERO)));
        assert_eq!(check(root, 16), Some(BalanceCheck::Failed));
        assert_eq!(check(root, 23), Some(BalanceCheck::Done(U256::ZERO)));
    });

    // Returning to a checked page shows its results without sending again.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.show_derive_preview_page(20, window, cx);
            root.show_derive_preview_page(0, window, cx);
        });
    });
    drive_a_while(&root, &runtime, cx);
    assert_eq!(page_reads(&selected_calls).len(), selected_indexes.len());
    root.read_with(cx, |root, _| {
        assert_eq!(preview(root).results.len(), selected_indexes.len());
        assert_eq!(check(root, 16), Some(BalanceCheck::Failed));
    });
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn derive_balance_check_stops_and_ends_with_the_dialog(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);
    let (endpoint, calls) = spawn_rpc_mock(1, Arc::default());
    use_balance_check_chains(&root, cx, &[(1, endpoint)]);
    open_derive(&root, cx);
    authorize(&root, &runtime, cx);
    let reads = || balance_reads(&calls).len();
    let start = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| root.start_derive_balance_check(window, cx));
        });
        cx.run_until_parked();
    };
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            for index in [15, 16, 17] {
                root.toggle_derive_preview_index(index, true, window, cx);
            }
        });
    });

    // Stop while the first address is in progress: nothing after it starts, and its result
    // doesn't land.
    start(cx);
    root.read_with(cx, |root, _| {
        let results = &preview(root).results;
        let loading = results
            .values()
            .filter(|check| **check == BalanceCheck::Loading)
            .count();
        assert_eq!((loading, results.len()), (1, 3));
    });
    cx.update(|window, cx| {
        root.update(cx, |root, cx| root.stop_derive_balance_check(window, cx));
    });
    drive_until(&root, &runtime, cx, |_| reads() == 1);
    drive_a_while(&root, &runtime, cx);
    assert_eq!(reads(), 1);
    root.read_with(cx, |root, _| {
        let preview = preview(root);
        assert!(preview.results.is_empty() && preview.checking.is_none());
    });

    // A page change during a run doesn't stop it.
    start(cx);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| root.show_derive_preview_page(20, window, cx));
    });
    assert!(root.read_with(cx, |root, _| preview(root).checking.is_some()));
    drive_until(&root, &runtime, cx, |root| preview(root).checking.is_none());
    assert_eq!(reads(), 4);
    root.read_with(cx, |root, _| {
        let results = &preview(root).results;
        assert!(
            [15, 16, 17]
                .iter()
                .all(|index| results.get(&(1, *index)) == Some(&BalanceCheck::Done(U256::ZERO)))
        );
    });

    // A chain change stops a run like Stop does.
    start(cx);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| root.set_derive_check_chain(10, window, cx));
    });
    drive_until(&root, &runtime, cx, |_| reads() == 5);
    drive_a_while(&root, &runtime, cx);
    assert_eq!(reads(), 5);
    root.read_with(cx, |root, _| assert!(preview(root).checking.is_none()));
    cx.update(|window, cx| {
        root.update(cx, |root, cx| root.set_derive_check_chain(1, window, cx));
    });

    // Closing the dialog from elsewhere ends the preview, its key and its check.
    start(cx);
    cx.update(gpui_component::WindowExt::close_all_dialogs);
    assert!(root.read_with(cx, |root, _| {
        root.public_form.derive_preview.preview.is_none()
    }));
    drive_until(&root, &runtime, cx, |_| reads() == 6);
    drive_a_while(&root, &runtime, cx);
    assert_eq!(reads(), 6);
    cx.update(|window, _| window.remove_window());
}

/// The compressed secp256k1 generator, standing in for the device's account key.
const ACCOUNT_KEY: [u8; 33] =
    alloy::primitives::hex!("0279BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798");
const CHAIN_CODE: [u8; 32] = [7; 32];
const LEDGER_PROFILE: &str = "ledger-profile-fingerprint";

fn device_keys() -> DerivedPublicAccountPreview {
    DerivedPublicAccountPreview::from_account_public_key("hw", &ACCOUNT_KEY, CHAIN_CODE).unwrap()
}

/// A Ledger stand-in. It answers with `ACCOUNT_KEY`, records each address it is asked to
/// confirm, and fails or confirms another address where told to.
#[derive(Default)]
struct StubDevice {
    read_error: Mutex<Option<String>>,
    reject: Mutex<Option<u32>>,
    mismatch: Mutex<Option<u32>>,
    asked: Mutex<Vec<HardwarePublicAccountDescriptor>>,
    /// When set, each confirmation waits for a permit.
    gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl StubDevice {
    fn asked(&self) -> Vec<u32> {
        let asked = self.asked.lock().unwrap();
        asked
            .iter()
            .map(|descriptor| descriptor.public_account_index)
            .collect()
    }
}

impl HardwarePublicAccountDevice for StubDevice {
    fn read_account_key(
        &self,
        request: HardwareDeviceRequest,
    ) -> HardwareDeviceResult<wallet_ops::hardware::HardwareAccountPublicKey> {
        let result = match self.read_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok((
                wallet_ops::hardware::HardwareAccountPublicKey {
                    public_key: ACCOUNT_KEY.to_vec(),
                    chain_code: CHAIN_CODE,
                },
                request.hardware_session,
            )),
        };
        Box::pin(async move { result })
    }

    fn confirm_account(
        &self,
        request: HardwareDeviceRequest,
        descriptor: HardwarePublicAccountDescriptor,
    ) -> HardwareDeviceResult<ConfirmedHardwarePublicAccount> {
        let index = descriptor.public_account_index;
        self.asked.lock().unwrap().push(descriptor.clone());
        let result = if *self.reject.lock().unwrap() == Some(index) {
            Err("Rejected on the device.".to_owned())
        } else {
            let address = if *self.mismatch.lock().unwrap() == Some(index) {
                Address::repeat_byte(0xee)
            } else {
                device_keys().address(index).unwrap()
            };
            Ok((
                ConfirmedHardwarePublicAccount::new_for_tests(descriptor, address),
                request.hardware_session,
            ))
        };
        let gate = self.gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            result
        })
    }
}

/// Stores a Ledger-derived wallet, selects it under its hardware profile session and puts
/// `device` in place of the Ledger.
fn use_ledger_wallet(
    root: &Entity<WalletRoot>,
    device: &Arc<StubDevice>,
    cx: &mut VisualTestContext,
) {
    use wallet_ops::hardware::{
        HardwareDerivationDescriptor, HardwareViewAccessKey, HardwareWalletSyncIntent,
        parse_bip32_path,
    };
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let store = root.vault_store.clone().unwrap();
            let descriptor = HardwareDerivationDescriptor::ledger_eip1024_v1(
                parse_bip32_path("m/44'/60'/0'/0/0").unwrap(),
                0,
                LEDGER_PROFILE.to_owned(),
                HardwareWalletSyncIntent::CreateNew,
            );
            let metadata = store
                .new_hardware_wallet_metadata(PASSWORD, "hw", "Ledger", descriptor)
                .unwrap();
            let access = HardwareViewAccessKey::new([9; 32]);
            store
                .store_hardware_derived_wallet_with_metadata(
                    PASSWORD,
                    "hw",
                    0,
                    &wallet_keys_from_mnemonic(MNEMONIC, "", 0).unwrap(),
                    &metadata,
                    &access,
                )
                .unwrap();
            let session = store
                .hardware_profile_session_for_fingerprint(
                    PASSWORD,
                    HardwareDeviceKind::Ledger,
                    LEDGER_PROFILE,
                    None,
                )
                .unwrap();
            root.view_session = Some(Arc::new(
                store
                    .load_hardware_view_session(PASSWORD, &session, "hw", &access)
                    .unwrap(),
            ));
            root.selected_wallet_id = Some(Arc::from("hw"));
            root.wallet_options = vec![WalletOption {
                wallet_id: Arc::from("hw"),
                source: WalletSource::LedgerDerived,
            }];
            root.public_form.derive_preview.device_stub =
                Some(Arc::clone(device) as Arc<dyn HardwarePublicAccountDevice>);
            root.reload_public_accounts(window, cx);
        });
    });
}

fn start_hardware_authorization(root: &Entity<WalletRoot>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.add_public_derived_account_from_input(window, cx);
        });
    });
}

/// The active Hardware accounts in the vault's order, by derivation index.
fn hardware_indexes(root: &WalletRoot) -> Vec<u32> {
    root.vault_store
        .as_ref()
        .unwrap()
        .list_all_public_accounts(root.view_session.as_ref().unwrap())
        .unwrap()
        .into_iter()
        .filter(|account| {
            account.source == PublicAccountSource::HardwareDerived
                && account.status == PublicAccountStatus::Active
        })
        .filter_map(|account| account.derivation_index)
        .collect()
}

/// Selects exactly `indexes` and adds them, driving the add until it ends.
fn add_hardware_indexes(
    root: &Entity<WalletRoot>,
    runtime: &tokio::runtime::Runtime,
    cx: &mut VisualTestContext,
    indexes: &[u32],
) {
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let selected = preview(root).selected.clone();
            for index in selected {
                root.toggle_derive_preview_index(index, false, window, cx);
            }
            for index in indexes {
                root.toggle_derive_preview_index(*index, true, window, cx);
            }
            root.add_selected_derived_public_accounts(window, cx);
        });
    });
    drive_until(root, runtime, cx, |root| {
        !root.public_form.derive_preview.adding()
    });
}

#[gpui::test]
fn hardware_derive_reads_the_device_without_a_password_and_ignores_late_reads(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);
    let device = Arc::new(StubDevice::default());
    use_ledger_wallet(&root, &device, cx);
    let mismatch = "wrong hardware device or passphrase context is active";
    *device.read_error.lock().unwrap() = Some(mismatch.to_owned());

    open_derive(&root, cx);
    assert!(
        cx.debug_bounds("public-derive-hardware-authorize")
            .is_some()
    );
    assert!(
        cx.debug_bounds("public-derive-authorize-password")
            .is_none()
    );
    // Enter performs Show addresses, and a failed read stays in authorize with its error.
    cx.simulate_keystrokes("enter");
    wait_for_authorization(&root, &runtime, cx);
    root.read_with(cx, |root, _| {
        assert!(root.public_form.derive_preview.preview.is_none());
        assert_eq!(root.public_form.error.as_deref(), Some(mismatch));
    });
    assert!(
        cx.debug_bounds("public-derive-hardware-authorize")
            .is_some()
    );

    // A read that returns after the dialog closes or the wallet changes opens nothing.
    *device.read_error.lock().unwrap() = None;
    open_derive(&root, cx);
    start_hardware_authorization(&root, cx);
    cx.update(gpui_component::WindowExt::close_all_dialogs);
    drive_a_while(&root, &runtime, cx);
    open_derive(&root, cx);
    start_hardware_authorization(&root, cx);
    root.update(cx, |root, _| root.advance_active_wallet_generation());
    drive_a_while(&root, &runtime, cx);
    root.read_with(cx, |root, _| {
        let state = &root.public_form.derive_preview;
        assert!(state.preview.is_none() && !state.authorizing);
    });

    open_derive(&root, cx);
    start_hardware_authorization(&root, cx);
    wait_for_authorization(&root, &runtime, cx);
    assert!(cx.debug_bounds("public-derive-preview").is_some());
    root.read_with(cx, |root, _| {
        let preview = preview(root);
        assert_eq!(
            preview.keys.page(0).unwrap(),
            device_keys().page(0).unwrap()
        );
        assert_eq!(preview.selected, BTreeSet::from([0]));
    });
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn hardware_add_confirms_each_index_in_order_and_skips_existing_ones(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);
    let device = Arc::new(StubDevice::default());
    use_ledger_wallet(&root, &device, cx);
    open_derive(&root, cx);
    start_hardware_authorization(&root, cx);
    wait_for_authorization(&root, &runtime, cx);

    // Index 7 gains an account after the preview opened, so the device isn't asked about it.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let store = root.vault_store.clone().unwrap();
            let view = root.view_session.clone().unwrap();
            let descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
                HardwareDeviceKind::Ledger,
                0,
                7,
            )
            .unwrap();
            let address = device_keys().address(7).unwrap();
            store
                .add_hardware_public_account(
                    &view,
                    &ConfirmedHardwarePublicAccount::new_for_tests(descriptor, address),
                    None,
                )
                .unwrap();
            root.reload_public_accounts(window, cx);
        });
    });
    add_hardware_indexes(&root, &runtime, cx, &[500, 7, 23, 4]);

    assert_eq!(device.asked(), [4, 23, 500]);
    for descriptor in device.asked.lock().unwrap().iter() {
        assert_eq!(
            *descriptor,
            HardwarePublicAccountDescriptor::for_wallet_public_index(
                HardwareDeviceKind::Ledger,
                0,
                descriptor.public_account_index,
            )
            .unwrap()
        );
    }
    root.read_with(cx, |root, _| {
        assert_eq!(hardware_indexes(root), [7, 4, 23, 500]);
        assert!(root.public_form.error.is_none());
        let preview = preview(root);
        assert!(preview.selected.is_empty());
        let added = preview.last_add.as_ref().unwrap();
        assert_eq!(added.added.len(), 3);
        assert_eq!(
            added.skipped,
            [(7, PreviewedPublicAccountSkip::AlreadyAdded)]
        );
    });
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn hardware_add_stops_on_rejection_or_a_different_address(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);
    let device = Arc::new(StubDevice::default());
    use_ledger_wallet(&root, &device, cx);
    open_derive(&root, cx);
    start_hardware_authorization(&root, cx);
    wait_for_authorization(&root, &runtime, cx);

    // A rejection keeps the account confirmed before it and asks for nothing after it.
    *device.reject.lock().unwrap() = Some(23);
    add_hardware_indexes(&root, &runtime, cx, &[4, 23, 500]);
    assert_eq!(device.asked(), [4, 23]);
    root.read_with(cx, |root, _| {
        assert_eq!(hardware_indexes(root), [4]);
        assert_eq!(
            root.public_form.error.as_deref(),
            Some("Rejected on the device. Added index 4. Not added: indexes 23, 500.")
        );
        assert_eq!(preview(root).selected, BTreeSet::from([23, 500]));
    });

    // An address other than the preview's isn't stored and stops the add.
    *device.reject.lock().unwrap() = None;
    *device.mismatch.lock().unwrap() = Some(23);
    device.asked.lock().unwrap().clear();
    add_hardware_indexes(&root, &runtime, cx, &[23, 500]);
    assert_eq!(device.asked(), [23]);
    root.read_with(cx, |root, _| {
        assert_eq!(hardware_indexes(root), [4]);
        assert!(root.public_form.error.as_deref().unwrap().starts_with(
            "The device showed a different address than the preview for index 23. It was not added."
        ));
    });
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn hardware_add_ends_with_the_dialog_or_the_hardware_session(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let _entered = runtime.enter();
    let (root, cx) = fixture(cx, directory.path(), &runtime);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let device = Arc::new(StubDevice {
        gate: Some(Arc::clone(&gate)),
        ..StubDevice::default()
    });
    use_ledger_wallet(&root, &device, cx);
    open_derive(&root, cx);
    start_hardware_authorization(&root, cx);
    wait_for_authorization(&root, &runtime, cx);

    // Closing the dialog while the device asks about index 4 stores nothing, even once the
    // device confirms, and asks about nothing after it.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.toggle_derive_preview_index(0, false, window, cx);
            root.toggle_derive_preview_index(4, true, window, cx);
            root.toggle_derive_preview_index(23, true, window, cx);
            root.add_selected_derived_public_accounts(window, cx);
        });
    });
    drive_until(&root, &runtime, cx, |_| device.asked() == [4]);
    cx.update(gpui_component::WindowExt::close_all_dialogs);
    gate.add_permits(10);
    drive_a_while(&root, &runtime, cx);
    assert_eq!(device.asked(), [4]);
    root.read_with(cx, |root, _| {
        assert_eq!(hardware_indexes(root), [] as [u32; 0]);
    });

    // A refreshed session keeps the preview. A replaced one ends it.
    open_derive(&root, cx);
    start_hardware_authorization(&root, cx);
    wait_for_authorization(&root, &runtime, cx);
    root.update(cx, |root, _| {
        let session = root.view_session.clone().unwrap();
        let state = &mut root.public_form.derive_preview;
        state.end_hardware_preview_unless(session.hardware_profile_session());
        assert!(state.preview.is_some());
        let mut replaced = session.hardware_profile_session().unwrap().clone();
        replaced.binding.fingerprint = "another device".to_owned();
        state.end_hardware_preview_unless(Some(&replaced));
        assert!(state.preview.is_none());
    });
    cx.update(|window, _| window.remove_window());
}
