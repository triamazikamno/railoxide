use super::*;

#[gpui::test]
fn shared_chain_editor_commits_discards_resets_and_preserves_stale_settings(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::{
        AppContext as _, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
        div,
    };
    use railgun_ui::chain_editor::{ChainDraft, ChainEditorCommand, ChainField, RpcEndpointDraft};
    use std::{cell::RefCell, rc::Rc};

    /// Mounts the shared editor under `Root`, which hosts its confirmation dialogs.
    struct ChainEditorHost(Entity<ui::chain_editor::ChainEditor>);

    impl Render for ChainEditorHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("shared-chain-editor");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut saved = WalletSettings::default();
    saved.runtime.auto_lock_timeout_secs = Some(600);
    wallet_ops::settings::save_wallet_settings(store.db().as_ref(), &saved).unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                saved.clone(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        let shared = editor.read(cx).chain_editor.clone();
        let host = cx.new(|_| ChainEditorHost(shared));
        *slot.borrow_mut() = Some(editor);
        gpui_component::Root::new(host, window, cx)
    });
    let owner = owner_slot.borrow_mut().take().unwrap();
    let cx = VisualTestContext::from_window(*handle, cx).into_mut();
    // The host has no scroll container, so the window must fit every built-in row plus the
    // custom chain at the bottom of the list.
    cx.simulate_resize(gpui::size(px(600.0), px(2000.0)));
    let click = |cx: &mut VisualTestContext, id: &'static str| {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("chain editor control {id} is rendered"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    };
    click(cx, "edit-chain-1");
    click(cx, "chain-enabled");
    click(cx, "chain-discard");
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        saved
    );
    let stored = || wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap();
    // Done stages the chain in the Settings draft, and only the footer's commit persists it.
    let commit_staged = |cx: &mut VisualTestContext, expected: &WalletSettings| {
        cx.update(|_, cx| {
            owner.update(cx, |editor, cx| {
                assert!(editor.is_dirty());
                assert_eq!(&editor.draft, expected);
                assert!(editor.persist_draft(cx));
            });
        });
    };
    click(cx, "edit-chain-1");
    click(cx, "chain-enabled");
    click(cx, "chain-save");
    assert_eq!(stored(), saved);
    saved.chains.per_chain.get_mut(&1).unwrap().enabled = false;
    commit_staged(cx, &saved);
    assert_eq!(stored(), saved);
    // Done returns to the list; reopen the chain before resetting it.
    click(cx, "edit-chain-1");
    click(cx, "chain-reset");
    // Reset is destructive, so it lands only after the confirmation dialog is confirmed.
    assert!(
        cx.debug_bounds("dialog-layer").is_some(),
        "reset opens a confirmation dialog before changing anything"
    );
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(stored(), saved);
    saved.chains.per_chain.remove(&1);
    commit_staged(cx, &saved);
    assert_eq!(stored(), saved);
    // A newly direct endpoint waits for confirmation, and declining saves nothing.
    click(cx, "chain-endpoint-0-direct");
    click(cx, "chain-save");
    assert!(
        cx.debug_bounds("chain-direct-confirm").is_some(),
        "a newly direct endpoint asks for confirmation before saving"
    );
    assert!(cx.debug_bounds("chain-submit-direct-confirm").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("chain-direct-confirm").is_none());
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        saved
    );
    click(cx, "chain-save");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(stored(), saved);
    cx.update(|_, cx| owner.update(cx, |editor, cx| assert!(editor.persist_draft(cx))));
    saved = stored();
    let endpoints = &saved.chains.per_chain[&1].rpc_endpoints;
    assert!(
        endpoints[0].direct && !endpoints[0].submit,
        "turning Direct on clears Submit"
    );
    assert!(
        endpoints[1..]
            .iter()
            .all(|endpoint| !endpoint.direct && endpoint.submit)
    );
    // Turning Submit back on for that Direct endpoint waits for its own confirmation.
    click(cx, "edit-chain-1");
    click(cx, "chain-endpoint-0-submit");
    click(cx, "chain-save");
    assert!(
        cx.debug_bounds("chain-submit-direct-confirm").is_some(),
        "submitting through a direct endpoint asks for confirmation before saving"
    );
    assert!(cx.debug_bounds("chain-direct-confirm").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        saved
    );
    // Reopen the confirmation so the host commit below lands while it is open.
    click(cx, "chain-save");
    assert!(cx.debug_bounds("chain-submit-direct-confirm").is_some());

    // Pricing can be saved without a network runtime and still preserves draft/revision isolation.
    cx.update(|window, cx| {
        owner.update(cx, |editor, cx| {
            let revision = wallet_ops::settings::settings_revision(&editor.saved)
                .unwrap()
                .to_string();
            let mut draft = wallet_ops::settings::chain_editor_draft(&editor.saved, 1).unwrap();
            draft.native_usd_pricing = railgun_ui::chain_editor::NativeUsdChoice::Disabled;
            let reply = editor
                .handle_chain_editor_command(
                    &revision,
                    &ChainEditorCommand::Save {
                        draft,
                        existing: true,
                    },
                    window,
                    cx,
                )
                .unwrap();
            assert!(!reply.restart_required);
            assert!(reply.draft.is_none());
            assert_eq!(
                editor.saved.chains.per_chain[&1].native_usd_pricing,
                wallet_ops::settings::NativeUsdPricing::Disabled
            );
        });
    });
    // Confirming the now-stale direct draft must not overwrite that concurrent commit.
    let concurrent = wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("chain-submit-direct-confirm").is_none());
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        concurrent
    );
    // The draft stays open and stale, so Save is disabled rather than reopening the dialog.
    click(cx, "chain-save");
    assert!(cx.debug_bounds("chain-submit-direct-confirm").is_none());
    // A Test validates the draft like Save but persists nothing and publishes no status.
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            let persisted =
                wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap();
            let before = editor.saved.clone();
            let mut draft = wallet_ops::settings::chain_editor_draft(&editor.saved, 1).unwrap();
            draft.native_usd_pricing = railgun_ui::chain_editor::NativeUsdChoice::Oracle;
            draft.fields.insert(
                ChainField::NativeUsdOracle,
                Address::repeat_byte(8).to_string(),
            );
            let target = editor.chain_editor.clone();
            editor.handle_chain_editor_probe(&draft, &target, cx);
            assert_eq!(editor.saved, before);
            assert_eq!(
                wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
                persisted
            );
        });
    });
    // A narrow addition must keep an older unrelated Settings draft, and its old save must fail.
    cx.update(|window, cx| {
        owner.update(cx, |editor, cx| {
            editor.draft.runtime.auto_lock_timeout_secs = Some(900);
            let revision = wallet_ops::settings::settings_revision(&editor.saved)
                .unwrap()
                .to_string();
            let mut draft = ChainDraft::new();
            draft.chain_id = "9007199254740993".into();
            draft.native_usd_pricing = railgun_ui::chain_editor::NativeUsdChoice::Oracle;
            draft.fields.insert(
                ChainField::NativeUsdOracle,
                Address::repeat_byte(7).to_string(),
            );
            for (field, value) in [
                (ChainField::Name, "Custom"),
                (ChainField::NativeName, "Custom coin"),
                (ChainField::NativeSymbol, "CSTM"),
                (ChainField::NativeDecimals, "6"),
            ] {
                draft.fields.insert(field, value.into());
            }
            draft.rpc_endpoints = vec![RpcEndpointDraft::new(
                "https://synthetic:credential@rpc.example".into(),
            )];
            editor
                .handle_chain_editor_command(
                    &revision,
                    &ChainEditorCommand::Save {
                        draft,
                        existing: false,
                    },
                    window,
                    cx,
                )
                .unwrap();
            assert_eq!(editor.draft.runtime.auto_lock_timeout_secs, Some(900));
            assert!(
                editor
                    .saved
                    .chains
                    .custom
                    .contains_key(&9_007_199_254_740_993)
            );
            assert!(!editor.persist_draft(cx));
            assert_eq!(editor.saved.runtime.auto_lock_timeout_secs, Some(600));
            // A storage read failure must not advance the owner's saved/active revision.
            let before = editor.saved.clone();
            let record = store.db().list_app_settings_records("").unwrap().remove(0);
            store
                .db()
                .put_app_settings_record(&record.key, &[0xc1])
                .unwrap();
            let revision = wallet_ops::settings::settings_revision(&before)
                .unwrap()
                .to_string();
            assert!(
                editor
                    .handle_chain_editor_command(
                        &revision,
                        &ChainEditorCommand::Remove {
                            chain_id: "9007199254740993".into(),
                        },
                        window,
                        cx
                    )
                    .is_err()
            );
            assert_eq!(editor.saved, before);
            assert_eq!(
                store
                    .db()
                    .get_app_settings_record(&record.key)
                    .unwrap()
                    .unwrap(),
                vec![0xc1]
            );
            store
                .db()
                .put_app_settings_record(&record.key, &record.payload)
                .unwrap();
            // The draft never sees outside commits, so dropping it picks up the addition.
            editor.discard_changes(cx);
        });
    });
    assert!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref())
            .unwrap()
            .chains
            .custom
            .contains_key(&9_007_199_254_740_993)
    );
    // Leaving the stale draft returns the shared list, which already carries that addition.
    click(cx, "chain-discard");
    assert!(
        cx.debug_bounds("edit-chain-9007199254740993").is_some(),
        "a commit made outside the editor refreshes the shared chain list"
    );
    click(cx, "edit-chain-9007199254740993");
    click(cx, "chain-save");
    assert!(
        cx.debug_bounds("edit-chain-9007199254740993").is_some(),
        "saving a custom chain returns to the list"
    );

    let endpoint = |settings: &WalletSettings, chain_id: u64, index: usize| {
        wallet_ops::settings::chain_editor_draft(settings, chain_id)
            .unwrap()
            .rpc_endpoints
            .swap_remove(index)
    };
    // Two built-in chains stage separately and persist in one footer commit.
    let before = stored();
    for chain in ["edit-chain-1", "edit-chain-56"] {
        click(cx, chain);
        click(cx, "chain-endpoint-1-general");
        click(cx, "chain-save");
    }
    assert_eq!(stored(), before);
    cx.update(|_, cx| owner.update(cx, |editor, cx| assert!(editor.persist_draft(cx))));
    saved = stored();
    for chain_id in [1, 56] {
        assert!(endpoint(&before, chain_id, 1).general);
        assert!(!endpoint(&saved, chain_id, 1).general);
    }

    // An Add chain form opened after an unrelated draft edit stages against the new draft.
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            editor.draft.runtime.auto_lock_timeout_secs = Some(900);
            editor.draft_changed(cx);
        });
    });
    click(cx, "chain-add");
    cx.simulate_input("424242");
    for value in ["Added", "Added coin", "ADD", "18"] {
        cx.simulate_keystrokes("tab");
        cx.simulate_input(value);
    }
    click(cx, "chain-add-endpoint");
    cx.simulate_input("https://added.example");
    click(cx, "chain-save");
    assert!(
        cx.debug_bounds("edit-chain-424242").is_some(),
        "the staged chain is listed"
    );
    click(cx, "edit-chain-1");
    click(cx, "chain-endpoint-1-sensitive");
    click(cx, "chain-save");
    assert_eq!(stored(), saved);
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            assert_eq!(editor.draft.runtime.auto_lock_timeout_secs, Some(900));
            assert!(editor.draft.chains.custom.contains_key(&424_242));
            assert!(!endpoint(&editor.draft, 1, 1).sensitive);
            editor.discard_changes(cx);
        });
    });
    // After Settings discard the list and the reopened chain show saved values again, so an
    // untouched Done stages nothing.
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("edit-chain-424242").is_none());
    click(cx, "edit-chain-1");
    click(cx, "chain-save");
    cx.update(|_, cx| {
        owner.update(cx, |editor, _| {
            assert!(!editor.is_dirty());
            assert_eq!(editor.draft, saved);
        });
    });

    // An outside commit leaves staged edits staging, and the footer then reports the conflict.
    click(cx, "edit-chain-1");
    click(cx, "chain-endpoint-1-sensitive");
    click(cx, "chain-save");
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            let revision = wallet_ops::settings::settings_revision(&editor.saved).unwrap();
            let mut draft = wallet_ops::settings::chain_editor_draft(&editor.saved, 137).unwrap();
            draft.native_usd_pricing = railgun_ui::chain_editor::NativeUsdChoice::Disabled;
            let candidate = wallet_ops::settings::chain_editor_mutation(
                &editor.saved,
                &ChainEditorCommand::Save {
                    draft,
                    existing: true,
                },
            )
            .unwrap()
            .prepare(&editor.saved)
            .unwrap();
            editor
                .commit_chain_candidate(revision, candidate, cx)
                .unwrap();
        });
    });
    let external = stored();
    assert_ne!(external, saved);
    click(cx, "edit-chain-56");
    click(cx, "chain-endpoint-1-sensitive");
    click(cx, "chain-save");
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            assert!(!endpoint(&editor.draft, 1, 1).sensitive);
            assert!(!endpoint(&editor.draft, 56, 1).sensitive);
            assert!(!editor.persist_draft(cx));
            editor.discard_changes(cx);
        });
    });
    assert_eq!(stored(), external);
    cx.simulate_resize(gpui::size(px(360.0), px(480.0)));
    cx.update(|window, cx| {
        window.set_rem_size(px(22.0));
        window.draw(cx).clear(cx);
    });
    // The fixed footer remains reachable when the advanced field body overflows.
    click(cx, "edit-chain-1");
    click(cx, "chain-discard");
    // Dropping the window and entities releases the test database before removal.
    cx.update(|window, _| window.remove_window());
    drop(owner);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}
