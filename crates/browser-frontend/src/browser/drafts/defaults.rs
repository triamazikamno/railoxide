use serde_json::Value;

pub(super) fn select_private_output(
    input: &mut Value,
    kind: &str,
    asset: &str,
    default_unwrap_asset: Option<&str>,
) {
    // Reopening the same selection must preserve an explicit output choice.
    if input["kind"] != kind || input["asset"] != asset || input.get("unwrap").is_none() {
        input["unwrap"] = (kind == "unshield" && default_unwrap_asset == Some(asset)).into();
        input["native_top_up"] = false.into();
    }
    input["kind"] = kind.into();
    input["asset"] = asset.into();
}

pub(super) fn select_public_mode(input: &mut Value, kind: &str, mimic_railway: bool) {
    if input["kind"] != kind || input.get("mimic_railway").is_none() {
        input["mimic_railway"] = mimic_railway.into();
    }
    input["kind"] = kind.into();
}
