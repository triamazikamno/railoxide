use super::*;
use gpui::{TestAppContext, VisualTestContext};

#[gpui::test]
fn saved_pricing_updates_preserve_keyboard_selected_source_and_unsaved_inputs(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_component::init);
    let mut editor = None;
    let handle = cx.add_window(|window, cx| {
        let mut snapshot = ChainEditorSnapshot::default();
        snapshot.revision = "saved-revision".into();
        let view = cx.new(|cx| ChainEditor::new(snapshot, cx));
        view.update(cx, |editor, cx| {
            let mut draft = ChainDraft::new();
            draft.chain_id = "1".into();
            draft.built_in = true;
            draft.railgun = true;
            draft.fields.insert(ChainField::NativeSymbol, "ETH".into());
            draft.fields.insert(ChainField::Name, "Ethereum".into());
            draft.fields.insert(
                ChainField::NativeUsdOracle,
                "0x0000000000000000000000000000000000000001".into(),
            );
            draft.defaults.insert(
                ChainField::NativeUsdOracle,
                draft.value(ChainField::NativeUsdOracle).into(),
            );
            editor.edit(draft, true, window, cx);
        });
        editor = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let editor = editor.unwrap();
    let cx = VisualTestContext::from_window(*handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(360.0), px(480.0)));
    cx.update(|window, cx| {
        let select = editor.read(cx).pricing_select.clone().unwrap();
        select.update(cx, |select, cx| select.focus(window, cx));
    });
    cx.simulate_keystrokes("enter down enter");
    cx.run_until_parked();
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            assert_eq!(
                editor.draft.as_ref().unwrap().native_usd_pricing,
                NativeUsdChoice::Oracle
            );
            let FieldInput::Single(input) = &editor.fields[&ChainField::NativeUsdOracle] else {
                unreachable!()
            };
            input.update(cx, |input, cx| {
                input.set_value("unfinished oracle", window, cx);
            });
            let mut status = NativeUsdStatus::default();
            status.state = NativeUsdState::Failed;
            status.reason = Some("Check the oracle contract".into());
            editor.set_pricing_status(BTreeMap::from([("1".into(), status)]), cx);
            assert_eq!(
                editor.fields[&ChainField::NativeUsdOracle].value(cx),
                "unfinished oracle"
            );
            assert_eq!(editor.snapshot.revision, "saved-revision");
            assert!(!editor.stale);
            assert!(editor.only_pricing_differs(editor.draft.as_ref().unwrap(), cx));
            let FieldInput::Single(input) = &editor.fields[&ChainField::BlockRange] else {
                unreachable!()
            };
            input.update(cx, |input, cx| input.set_value("123", window, cx));
            assert!(!editor.only_pricing_differs(editor.draft.as_ref().unwrap(), cx));
        });
        window.set_rem_size(px(22.0));
        window.draw(cx).clear(cx);
    });
    let save = cx
        .debug_bounds("chain-save")
        .expect("save stays rendered at enlarged scale");
    assert!(save.bottom() <= px(480.0));
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.retire(cx);
            assert!(editor.pricing_status.is_empty());
            assert!(!editor.is_editing());
            window.remove_window();
        });
    });
}

#[gpui::test]
fn back_asks_before_dropping_edits_and_only_then(cx: &mut TestAppContext) {
    use gpui_component::WindowExt as _;
    cx.update(gpui_component::init);
    let mut editor = None;
    // No revision, so Back's List request leaves nothing pending without a host.
    let handle = cx.add_window(|window, cx| {
        let view = cx.new(|cx| ChainEditor::new(ChainEditorSnapshot::default(), cx));
        editor = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let editor = editor.unwrap();
    let cx = VisualTestContext::from_window(*handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(360.0), px(480.0)));
    let open = |cx: &mut VisualTestContext, draft: ChainDraft, existing: bool| {
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| editor.edit(draft, existing, window, cx));
        });
        cx.run_until_parked();
    };
    let click_back = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let back = cx.debug_bounds("chain-back").expect("back is rendered");
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    };

    let mut custom = ChainDraft::new();
    custom.chain_id = "8453".into();
    custom.fields.insert(ChainField::Name, "Custom".into());
    custom.fields.insert(ChainField::NativeSymbol, "ETH".into());
    custom.rpc_endpoints = vec![RpcEndpointDraft::new("https://rpc.example".into())];
    open(cx, custom.clone(), true);
    cx.update(|window, cx| {
        let input = editor.read(cx).rpc_endpoints[0].0.clone();
        input.update(cx, |input, cx| {
            input.set_value("https://edited.example", window, cx);
        });
    });
    click_back(cx);
    cx.update(|window, cx| {
        assert!(window.has_active_dialog(cx));
        assert!(editor.read(cx).is_editing());
        window.close_dialog(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        assert!(!window.has_active_dialog(cx));
        let editor = editor.read(cx);
        assert!(editor.is_editing());
        assert_eq!(
            editor.collected_endpoints(cx)[0].url,
            "https://edited.example"
        );
    });

    // An untouched form has inputs the opened draft lacks, which must not count as edits.
    for (draft, existing) in [(ChainDraft::new(), false), (custom, true)] {
        open(cx, draft, existing);
        click_back(cx);
        cx.update(|window, cx| {
            assert!(!window.has_active_dialog(cx));
            assert!(!editor.read(cx).is_editing());
        });
    }
    cx.update(|window, _| window.remove_window());
}
