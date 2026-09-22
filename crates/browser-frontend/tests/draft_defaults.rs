#![cfg(not(target_family = "wasm"))]

use serde_json::json;

#[path = "../src/browser/drafts/defaults.rs"]
mod defaults;

#[test]
fn unshield_defaults_follow_asset_and_mode_changes_but_preserve_manual_output() {
    let wrapped = "0x1111111111111111111111111111111111111111";
    let other = "0x2222222222222222222222222222222222222222";
    let mut input = json!({"kind":"unshield", "asset":wrapped});
    defaults::select_private_output(&mut input, "unshield", wrapped, Some(wrapped));
    assert_eq!(input["unwrap"], true);

    input["unwrap"] = false.into();
    input["native_top_up"] = true.into();
    defaults::select_private_output(&mut input, "unshield", wrapped, Some(wrapped));
    assert_eq!(input["unwrap"], false);
    assert_eq!(input["native_top_up"], true);

    defaults::select_private_output(&mut input, "unshield", other, Some(wrapped));
    assert_eq!(input["unwrap"], false);
    assert_eq!(input["native_top_up"], false);
    defaults::select_private_output(&mut input, "unshield", wrapped, Some(wrapped));
    assert_eq!(input["unwrap"], true);

    defaults::select_private_output(&mut input, "private_send", wrapped, Some(wrapped));
    assert_eq!(input["unwrap"], false);
    defaults::select_private_output(&mut input, "unshield", wrapped, Some(wrapped));
    assert_eq!(input["unwrap"], true);

    let mut input = json!({"kind":"unshield", "asset":wrapped});
    defaults::select_private_output(&mut input, "unshield", wrapped, None);
    assert_eq!(input["unwrap"], false);
}

#[test]
fn shield_defaults_follow_saved_preference_but_preserve_manual_profile() {
    for saved in [false, true] {
        let mut input = json!({"kind":"shield"});
        defaults::select_public_mode(&mut input, "shield", saved);
        assert_eq!(input["mimic_railway"], saved);

        input["mimic_railway"] = (!saved).into();
        defaults::select_public_mode(&mut input, "shield", saved);
        assert_eq!(input["mimic_railway"], !saved);

        defaults::select_public_mode(&mut input, "send", saved);
        defaults::select_public_mode(&mut input, "shield", saved);
        assert_eq!(input["mimic_railway"], saved);
    }
}
