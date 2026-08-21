use super::{helpers::parse_caip2_chain_id, *};
use gpui::StatefulInteractiveElement;
use gpui_component::tooltip::Tooltip;

pub(super) fn walletconnect_title_row(label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(walletconnect_logo(px(18.0)))
        .child(app_strong_text(label))
}

pub(in crate::root) fn walletconnect_logo(size: Pixels) -> gpui::Div {
    walletconnect_logo_with_color(size, WALLETCONNECT_BLUE)
}

pub(in crate::root) fn walletconnect_logo_with_presence(
    size: Pixels,
    has_active_session: bool,
) -> gpui::AnyElement {
    walletconnect_logo_with_badges(size, has_active_session, 0)
}

pub(in crate::root) fn walletconnect_logo_with_badges(
    size: Pixels,
    has_active_session: bool,
    pending_request_count: usize,
) -> gpui::AnyElement {
    let mut logo = if has_active_session {
        walletconnect_active_logo(size).into_any_element()
    } else {
        walletconnect_logo_hoverable(size).into_any_element()
    };
    if pending_request_count > 0 {
        logo = Badge::new()
            .count(pending_request_count)
            .color(rgb(theme::DANGER))
            .child(logo)
            .into_any_element();
    }
    logo
}

pub(super) fn walletconnect_active_logo(size: Pixels) -> gpui::Div {
    let dot_size = if size <= px(16.0) { px(6.0) } else { px(8.0) };
    div()
        .relative()
        .size(size)
        .flex()
        .flex_none()
        .child(walletconnect_logo_hoverable(size))
        .child(
            div()
                .absolute()
                .right_0()
                .bottom_0()
                .size(dot_size)
                .rounded_full()
                .border_1()
                .border_color(rgb(theme::SURFACE))
                .bg(rgb(theme::PRESENCE_ONLINE)),
        )
}

pub(super) fn walletconnect_logo_hoverable(size: Pixels) -> gpui::Div {
    walletconnect_logo(size).hover(|this| this.bg(rgb(WALLETCONNECT_BLUE_HOVER)))
}

pub(super) fn walletconnect_logo_with_color(size: Pixels, color: u32) -> gpui::Div {
    div()
        .size(size)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(color))
        .child(img(WALLETCONNECT_ICON_PATH).size(size).flex_none())
}

pub(super) fn walletconnect_subpanel(title: &'static str) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb_with_alpha(theme::SURFACE_HOVER_SUBTLE, 0.45))
        .p(px(10.0))
        .child(
            app_strong_text(title)
                .text_size(px(13.0))
                .text_color(rgb(theme::TEXT)),
        )
}

pub(super) fn walletconnect_disclosure_content(content: gpui::Div) -> gpui::Div {
    div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .pl(px(14.0))
        .pt(px(8.0))
        .pb(px(7.0))
        .child(content)
}

pub(super) fn render_walletconnect_approval_stepper(
    progress: &WalletConnectApprovalProgress,
) -> gpui::Div {
    let mut stepper = app_stepper_container();
    let last_index = progress.steps.len().saturating_sub(1);
    for (index, step) in progress.steps.iter().enumerate() {
        stepper = stepper.child(render_walletconnect_approval_step(
            step,
            index == last_index,
        ));
    }
    stepper
}

pub(super) fn render_walletconnect_approval_step(
    step: &WalletConnectApprovalStepState,
    is_last: bool,
) -> gpui::Div {
    let color = public_action_step_color(step.status);
    let mut body = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .pb(if is_last { px(0.0) } else { px(12.0) })
        .child(
            app_strong_text(walletconnect_approval_step_label(step.step))
                .text_color(rgb(color))
                .line_height(gpui::relative(1.0)),
        );
    let detail = step
        .message
        .as_deref()
        .unwrap_or_else(|| walletconnect_approval_step_detail(step.step, step.status));
    body = body.child(
        app_muted_text(detail.to_owned())
            .text_color(rgb(color))
            .line_height(gpui::relative(1.0))
            .whitespace_normal(),
    );
    if let Some(tx_hash) = step.tx_hash.as_ref() {
        body = body.child(
            app_muted_text(format!("Tx {}", short_hash(tx_hash)))
                .font_family(APP_MONO_FONT_FAMILY)
                .line_height(gpui::relative(1.0)),
        );
    }

    app_step_row(
        render_public_action_step_marker(step.status, color),
        body,
        is_last,
        color,
        px(32.0),
        None,
    )
}

pub(super) fn walletconnect_approval_progress_steps(
    request: &WalletConnectRequestUi,
) -> Vec<WalletConnectApprovalProgressStep> {
    match &request.parsed {
        WalletConnectParsedRequest::EthSendTransaction { .. } => vec![
            WalletConnectApprovalProgressStep::PrepareRequest,
            WalletConnectApprovalProgressStep::ApproveOnDevice,
            WalletConnectApprovalProgressStep::BroadcastTransaction,
            WalletConnectApprovalProgressStep::RespondToDapp,
        ],
        WalletConnectParsedRequest::PersonalSign { .. }
        | WalletConnectParsedRequest::EthSignTypedData { .. }
        | WalletConnectParsedRequest::EthSignTypedDataV4 { .. } => vec![
            WalletConnectApprovalProgressStep::ApproveOnDevice,
            WalletConnectApprovalProgressStep::RespondToDapp,
        ],
        WalletConnectParsedRequest::EthAccounts
        | WalletConnectParsedRequest::EthRequestAccounts
        | WalletConnectParsedRequest::WalletSwitchEthereumChain { .. } => {
            vec![WalletConnectApprovalProgressStep::RespondToDapp]
        }
    }
}

const fn walletconnect_approval_step_label(
    step: WalletConnectApprovalProgressStep,
) -> &'static str {
    match step {
        WalletConnectApprovalProgressStep::PrepareRequest => "Prepare request",
        WalletConnectApprovalProgressStep::ApproveOnDevice => "Approve on device",
        WalletConnectApprovalProgressStep::BroadcastTransaction => "Broadcast transaction",
        WalletConnectApprovalProgressStep::RespondToDapp => "Respond to dapp",
    }
}

const fn walletconnect_approval_step_detail(
    step: WalletConnectApprovalProgressStep,
    status: PublicActionStepStatus,
) -> &'static str {
    match status {
        PublicActionStepStatus::NotStarted => match step {
            WalletConnectApprovalProgressStep::PrepareRequest => {
                "Waiting to validate the request and fill transaction fields."
            }
            WalletConnectApprovalProgressStep::ApproveOnDevice => "Waiting for device approval.",
            WalletConnectApprovalProgressStep::BroadcastTransaction => {
                "Waiting to broadcast the signed transaction."
            }
            WalletConnectApprovalProgressStep::RespondToDapp => {
                "Waiting to publish the WalletConnect response."
            }
        },
        PublicActionStepStatus::Pending => match step {
            WalletConnectApprovalProgressStep::PrepareRequest => {
                "Validating the request and preparing transaction fields."
            }
            WalletConnectApprovalProgressStep::ApproveOnDevice => {
                "Confirm this WalletConnect request on your hardware wallet."
            }
            WalletConnectApprovalProgressStep::BroadcastTransaction => {
                "Broadcasting the signed transaction through configured public RPC."
            }
            WalletConnectApprovalProgressStep::RespondToDapp => {
                "Publishing the encrypted WalletConnect response to the dapp."
            }
        },
        PublicActionStepStatus::Done => match step {
            WalletConnectApprovalProgressStep::PrepareRequest => "Request prepared.",
            WalletConnectApprovalProgressStep::ApproveOnDevice => "Device approval completed.",
            WalletConnectApprovalProgressStep::BroadcastTransaction => "Transaction broadcast.",
            WalletConnectApprovalProgressStep::RespondToDapp => "Dapp response published.",
        },
        PublicActionStepStatus::Error => "Failed.",
        PublicActionStepStatus::Stopped => "Stopped locally.",
    }
}

#[cfg(feature = "hardware")]
pub(super) fn walletconnect_trezor_app_passphrase_input(
    input: &Entity<InputState>,
    disabled: bool,
) -> gpui::Div {
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .child(app_strong_text("Trezor app passphrase"))
        .child(
            app_muted_text(
                "If the Trezor session expired, enter the app passphrase for this request.",
            )
            .whitespace_normal(),
        )
        .child(app_input(input).disabled(disabled))
}

pub(super) fn walletconnect_privacy_notices() -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(Alert::warning(
            "walletconnect-privacy-warning",
            "Connecting exposes this Public account address to the dapp for approved chains. Private Railgun and shielded wallet material is not exposed. In a normal browser, the dapp may still link this Public account to IP, cookies, fingerprint, or site activity.",
        ).small())
}

pub(super) fn walletconnect_metadata_block(metadata: &WalletConnectPeerMetadata) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(walletconnect_kv_row("Dapp", metadata.name.clone()))
        .child(walletconnect_kv_row("URL", metadata.url.clone()))
        .when(!metadata.description.is_empty(), |this| {
            this.child(walletconnect_kv_row(
                "Description",
                metadata.description.clone(),
            ))
        })
}

pub(super) fn walletconnect_kv_row(label: &'static str, value: String) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .text_size(APP_TEXT_SIZE)
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(label),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .text_align(gpui::TextAlign::Right)
                .text_color(rgb(theme::TEXT))
                .whitespace_normal()
                .child(SharedString::from(value)),
        )
}

pub(super) fn walletconnect_kv_element_row(
    label: &'static str,
    value: impl IntoElement,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .text_size(APP_TEXT_SIZE)
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(label),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .flex()
                .justify_end()
                .child(value),
        )
}

pub(super) fn walletconnect_completed_tx_hash_row(request_key: &str, tx_hash: &str) -> gpui::Div {
    let copy_id = SharedString::from(format!(
        "walletconnect-request-result-tx-copy-{request_key}"
    ));
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(APP_TEXT_SIZE)
                .text_color(rgb(theme::TEXT_MUTED))
                .child("Transaction hash"),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_start()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .bg(rgb(theme::SURFACE))
                .p(px(8.0))
                .child(
                    app_muted_text(tx_hash.to_owned())
                        .min_w(px(0.0))
                        .flex_1()
                        .font_family(APP_MONO_FONT_FAMILY)
                        .whitespace_normal(),
                )
                .child(clipboard_with_toast(copy_id, tx_hash.to_owned())),
        )
}

pub(super) fn walletconnect_approved_chains_row(session: &WalletConnectSessionRecord) -> gpui::Div {
    let items = approved_chain_display_items(session);
    let mut chains = div()
        .min_w(px(0.0))
        .flex()
        .flex_wrap()
        .justify_end()
        .gap_2()
        .text_color(rgb(theme::TEXT));
    if items.is_empty() {
        chains = chains.child("None");
    } else {
        for item in items {
            chains = chains.child(walletconnect_approved_chain_chip(&item));
        }
    }
    walletconnect_kv_element_row("Approved chains", chains)
}

pub(super) fn walletconnect_approved_chain_chip(
    item: &WalletConnectApprovedChainDisplay,
) -> gpui::Div {
    let mut chip = div()
        .flex()
        .items_center()
        .gap_1()
        .whitespace_nowrap()
        .text_color(rgb(theme::TEXT))
        .text_size(APP_TEXT_SIZE);
    if let Some(path) = item.icon_path {
        chip = chip.child(img(path).size(px(16.0)).flex_none());
    }
    chip.child(SharedString::from(item.label.clone()))
}

pub(super) fn walletconnect_raw_details(request_key: &str, value: &Value) -> gpui::Div {
    let raw = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let copy_action_id = SharedString::from(format!(
        "walletconnect-request-raw-copy-action-{request_key}"
    ));
    let copy_id = SharedString::from(format!("walletconnect-request-raw-copy-{request_key}"));
    walletconnect_disclosure_content(
        div()
            .w_full()
            .min_w(px(0.0))
            .relative()
            .child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .right(px(0.0))
                    .id(copy_action_id)
                    .tooltip(|window, cx| Tooltip::new("Copy raw request").build(window, cx))
                    .child(clipboard_with_toast(copy_id, raw.clone())),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .pr(px(28.0))
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_size(px(12.0))
                    .line_height(px(16.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .whitespace_normal()
                    .child(SharedString::from(raw)),
            ),
    )
}

pub(super) fn short_uuid(value: &str) -> String {
    value.chars().take(12).collect()
}

pub(super) fn walletconnect_unresolved_public_account_label(
    session: &WalletConnectSessionRecord,
) -> String {
    if session.lifecycle_state == WalletConnectSessionLifecycleState::TemporarilyPaused {
        "Account from another wallet".to_owned()
    } else {
        short_uuid(&session.selected_public_account_uuid)
    }
}

pub(super) fn chain_label_for_caip2(chain_id: &str) -> String {
    parse_caip2_chain_id(chain_id).map_or_else(
        || chain_id.to_owned(),
        |chain| {
            chain_name(chain).map_or_else(
                || chain_id.to_owned(),
                |name| format!("{name} ({chain_id})"),
            )
        },
    )
}

pub(super) fn approved_chain_display_items(
    session: &WalletConnectSessionRecord,
) -> Vec<WalletConnectApprovedChainDisplay> {
    session
        .approved_namespaces
        .values()
        .flat_map(|namespace| namespace.chains.iter())
        .map(|chain| approved_chain_display_item(chain))
        .map(|item| (item.label.to_ascii_lowercase(), item))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

pub(super) fn approved_chain_display_item(chain_id: &str) -> WalletConnectApprovedChainDisplay {
    parse_caip2_chain_id(chain_id).map_or_else(
        || WalletConnectApprovedChainDisplay {
            label: chain_id.to_owned(),
            icon_path: None,
        },
        |chain| WalletConnectApprovedChainDisplay {
            label: chain_name(chain).map_or_else(|| chain_id.to_owned(), str::to_owned),
            icon_path: chain_icon_asset_path(chain),
        },
    )
}

pub(super) fn walletconnect_lifecycle_label(state: WalletConnectSessionLifecycleState) -> String {
    match state {
        WalletConnectSessionLifecycleState::Active => "Active",
        WalletConnectSessionLifecycleState::TemporarilyPaused => "Paused: switch to owning wallet",
        WalletConnectSessionLifecycleState::Invalid => "Invalid Public account",
        WalletConnectSessionLifecycleState::Disconnected => "Disconnected",
        WalletConnectSessionLifecycleState::Expired => "Expired",
    }
    .to_owned()
}

pub(super) fn format_unix_seconds(timestamp: u64) -> String {
    let Ok(seconds) = i64::try_from(timestamp) else {
        return timestamp.to_string();
    };
    Local.timestamp_opt(seconds, 0).single().map_or_else(
        || timestamp.to_string(),
        |time| time.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
    )
}
