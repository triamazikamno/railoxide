use super::*;

#[gpui::test]
fn pinned_form_keeps_its_pin_until_the_user_accepts_leaving_it(cx: &mut gpui::TestAppContext) {
    let path = std::env::temp_dir().join(format!(
        "pinned-private-form-{:032x}",
        rand::random::<u128>()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::repeat_byte(0x55),
        label: "TEST".into(),
        decimals: Some(18),
        total: U256::from(1_000),
        poi_verified_total: U256::from(1_000),
        max_batched: U256::from(1_000),
        icon_path: None,
    };
    let key = UnshieldAssetKey::from_asset(&asset);
    let pin = wallet_ops::PublicBroadcasterInputPin {
        inputs: vec![wallet_ops::SelectedInputIdentity {
            tree: 0,
            position: 7,
        }],
        overridden: false,
    };
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.initialize_send_form(asset, window, cx);
            root.send_forms.get_mut(&key).unwrap().input_pin = Some(pin.clone());
            let amount = root.send_forms[&key].amount_input.clone();
            amount.update(cx, |input, cx| input.set_value("0.5", window, cx));
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            assert_eq!(root.send_forms[&key].input_pin, Some(pin.clone()));
            root.set_send_delivery_mode(key, DeliveryMode::ManualCalldata, window, cx);
            assert!(window.has_active_dialog(cx), "leaving the pin asks first");
            assert_eq!(
                root.send_forms[&key].delivery_mode,
                DeliveryMode::PublicBroadcaster
            );
        });
        // Cancel leaves the form as it was.
        window.close_all_dialogs(cx);
        root.update(cx, |root, cx| {
            assert_eq!(
                root.send_forms[&key].delivery_mode,
                DeliveryMode::PublicBroadcaster
            );
            assert_eq!(root.send_forms[&key].input_pin, Some(pin.clone()));
            root.set_send_delivery_mode(key, DeliveryMode::ManualCalldata, window, cx);
        });
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
    cx.run_until_parked();
    root.read_with(cx, |root, _| {
        let form = &root.send_forms[&key];
        assert_eq!(form.delivery_mode, DeliveryMode::ManualCalldata);
        assert_eq!(form.input_pin, None, "accepting the risk clears the pin");
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    drop(entered);
    drop(runtime);
    std::fs::remove_dir_all(path).unwrap();
}
