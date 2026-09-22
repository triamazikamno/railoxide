use super::{
    ANCHOR_BPS_SLIDER_MAX, ANCHOR_BPS_SLIDER_MAX_BPS, ANCHOR_BPS_SLIDER_MIN,
    AUTO_LOCK_TIMEOUT_PRESETS_SECS, Pixels, SharedString, WalletSettings, Window, px,
};

pub(in crate::root) fn settings_dialog_dimensions(window: &Window) -> (Pixels, Pixels, Pixels) {
    let viewport = window.viewport_size();
    let width = (viewport.width * 0.94).min(px(920.0));
    let content_height = (viewport.height - px(120.0)).max(px(180.0)).min(px(620.0));
    let max_height = (viewport.height - px(32.0)).max(px(240.0));
    (width, content_height, max_height)
}

const AUTO_LOCK_DISABLED_VALUE: &str = "disabled";

pub(in crate::root) fn auto_lock_timeout_options() -> Vec<(SharedString, SharedString)> {
    std::iter::once((
        SharedString::from(AUTO_LOCK_DISABLED_VALUE),
        SharedString::from("Disabled"),
    ))
    .chain(AUTO_LOCK_TIMEOUT_PRESETS_SECS.iter().map(|timeout| {
        (
            SharedString::from(timeout.to_string()),
            SharedString::from(format_auto_lock_timeout(*timeout)),
        )
    }))
    .collect()
}

pub(in crate::root) fn auto_lock_timeout_value(timeout: Option<u64>) -> SharedString {
    timeout.map_or_else(
        || SharedString::from(AUTO_LOCK_DISABLED_VALUE),
        |timeout| SharedString::from(timeout.to_string()),
    )
}

pub(in crate::root) fn auto_lock_timeout_from_value(value: &str) -> Option<u64> {
    (value != AUTO_LOCK_DISABLED_VALUE)
        .then(|| value.parse().ok())
        .flatten()
}

fn format_auto_lock_timeout(timeout_secs: u64) -> String {
    if timeout_secs.is_multiple_of(60 * 60) {
        let hours = timeout_secs / (60 * 60);
        format!("{hours} hour{}", if hours == 1 { "" } else { "s" })
    } else {
        let minutes = timeout_secs / 60;
        format!("{minutes} minutes")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SettingsApplyMode {
    Clean,
    NetworkingRestart,
    NewRequests,
    FutureSessions,
}

pub(in crate::root) fn classify_settings_apply_mode(
    saved: &WalletSettings,
    draft: &WalletSettings,
) -> SettingsApplyMode {
    if draft == saved {
        return SettingsApplyMode::Clean;
    }
    if draft.network != saved.network
        || built_in_operations_changed(saved, draft)
        || draft.indexed_artifacts != saved.indexed_artifacts
        || draft.poi != saved.poi
        || draft.waku != saved.waku
    {
        SettingsApplyMode::NetworkingRestart
    } else if draft.chains != saved.chains
        || draft.privacy != saved.privacy
        || draft.broadcaster != saved.broadcaster
        || draft.gas != saved.gas
        || draft.walletconnect != saved.walletconnect
    {
        SettingsApplyMode::NewRequests
    } else {
        SettingsApplyMode::FutureSessions
    }
}

fn built_in_operations_changed(saved: &WalletSettings, draft: &WalletSettings) -> bool {
    railgun_ui::built_in_chain_ids().any(|id| {
        let previous = saved.chains.per_chain.get(&id).cloned().unwrap_or_default();
        let mut next = draft.chains.per_chain.get(&id).cloned().unwrap_or_default();
        next.native_usd_pricing = previous.native_usd_pricing;
        next != previous
    })
}

pub(in crate::root) fn settings_save_action_enabled(
    saved: &WalletSettings,
    draft: &WalletSettings,
    has_validation_error: bool,
) -> bool {
    !has_validation_error
        && draft != saved
        && classify_settings_apply_mode(saved, draft) != SettingsApplyMode::NetworkingRestart
}

pub(in crate::root) fn settings_restart_action_enabled(
    saved: &WalletSettings,
    draft: &WalletSettings,
    has_validation_error: bool,
    root_replacement_allowed: bool,
) -> bool {
    root_replacement_allowed && !has_validation_error && draft != saved
}

pub(in crate::root) fn settings_restart_reuses_active_network(
    saved: &WalletSettings,
    draft: &WalletSettings,
) -> bool {
    saved.network == draft.network
}

pub(in crate::root) const fn broadcaster_anchor_bps_range(settings: &WalletSettings) -> (u64, u64) {
    let min_bps = settings.broadcaster.min_anchor_bps;
    let max_bps = settings.broadcaster.max_anchor_bps;
    if min_bps <= max_bps {
        (min_bps, max_bps)
    } else {
        (max_bps, min_bps)
    }
}

pub(in crate::root) const fn set_broadcaster_anchor_bps_range(
    settings: &mut WalletSettings,
    start: f32,
    end: f32,
) {
    let min_bps = anchor_slider_value_to_bps(start.min(end));
    let max_bps = anchor_slider_value_to_bps(start.max(end));
    settings.broadcaster.min_anchor_bps = min_bps;
    settings.broadcaster.max_anchor_bps = max_bps;
}

#[allow(clippy::cast_precision_loss)]
pub(in crate::root) fn anchor_bps_to_slider_value(bps: u64) -> f32 {
    bps.min(ANCHOR_BPS_SLIDER_MAX_BPS) as f32
}

#[allow(clippy::cast_sign_loss)]
pub(in crate::root) const fn anchor_slider_value_to_bps(value: f32) -> u64 {
    value
        .round()
        .clamp(ANCHOR_BPS_SLIDER_MIN, ANCHOR_BPS_SLIDER_MAX) as u64
}

pub(in crate::root) fn format_anchor_bps_percent(bps: u64) -> String {
    let whole = bps / 100;
    let fractional = bps % 100;
    if fractional == 0 {
        format!("{whole}%")
    } else if fractional.is_multiple_of(10) {
        format!("{whole}.{}%", fractional / 10)
    } else {
        format!("{whole}.{fractional:02}%")
    }
}

pub(in crate::root) fn format_anchor_bps_percent_range(min_bps: u64, max_bps: u64) -> String {
    format!(
        "{} - {} of price anchor",
        format_anchor_bps_percent(min_bps),
        format_anchor_bps_percent(max_bps)
    )
}

pub(in crate::root) fn format_anchor_premium_range(min_bps: u64, max_bps: u64) -> String {
    format!(
        "Allows {} to {} vs anchor",
        format_anchor_premium_bps(min_bps),
        format_anchor_premium_bps(max_bps)
    )
}

pub(in crate::root) fn format_anchor_premium_bps(bps: u64) -> String {
    let premium = i128::from(bps) - 10_000;
    if premium == 0 {
        return "0%".to_string();
    }
    let sign = if premium > 0 { "+" } else { "-" };
    let abs_bps = premium.unsigned_abs();
    format!("{sign}{}", format_anchor_bps_percent(abs_bps as u64))
}

pub(in crate::root) fn format_anchor_bps_exact_range(min_bps: u64, max_bps: u64) -> String {
    format!(
        "{} - {} bps",
        format_u64_grouped(min_bps),
        format_u64_grouped(max_bps)
    )
}

pub(in crate::root) fn format_u64_grouped(value: u64) -> String {
    let raw = value.to_string();
    let mut formatted = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().enumerate() {
        if index > 0 && (raw.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted
}

pub(in crate::root) fn settings_draft_after_discard(saved: &WalletSettings) -> WalletSettings {
    saved.clone()
}
