#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn hardware_setup_notice(device_kind: HardwareDeviceKind) -> gpui::Div {
    let [connect, passphrase, sync_baseline, custody] = hardware_setup_notice_lines(device_kind);
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb_with_alpha(theme::SURFACE, 0.72))
        .child(app_strong_text("Before approving on the device"))
        .child(app_muted_text(connect))
        .child(app_muted_text(passphrase))
        .child(app_muted_text(sync_baseline))
        .child(app_muted_text(custody).text_color(rgb(theme::WARNING)))
}

pub(super) fn hardware_setup_progress(device_kind: HardwareDeviceKind) -> gpui::Div {
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::INFO))
        .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.74))
        .child(
            app_strong_text(hardware_setup_progress_title(device_kind))
                .text_color(rgb(theme::INFO)),
        )
        .child(app_muted_text(hardware_setup_progress_detail(device_kind)).whitespace_normal())
}

pub(in crate::root) fn hardware_setup_progress_title(device_kind: HardwareDeviceKind) -> String {
    let device_label = hardware_device_label(device_kind);
    format!("Waiting for {device_label} approval")
}

pub(in crate::root) fn hardware_setup_progress_detail(device_kind: HardwareDeviceKind) -> String {
    let device_label = hardware_device_label(device_kind);
    match device_kind {
        HardwareDeviceKind::Ledger => format!(
            "Check your {device_label} and approve the Railgun derivation request. The device may describe this as providing a public key or shared secret."
        ),
        HardwareDeviceKind::Trezor => {
            format!("Check your {device_label} and approve the Railgun derivation request.")
        }
    }
}

pub(in crate::root) fn hardware_setup_notice_lines(device_kind: HardwareDeviceKind) -> [String; 4] {
    let device_label = hardware_device_label(device_kind);
    [
        format!(
            "Connect your {device_label}, open the Ethereum app, and approve the Railgun derivation request."
        ),
        "If you use a hardware passphrase wallet, activate the intended passphrase context on the device. Do not enter that passphrase into this app.".to_owned(),
        "Create new starts from the current safe head. Recover existing backfills from deployment and is safer if unsure.".to_owned(),
        "This is hardware-derived software custody, not true hardware signing: the desktop app signs Railgun spends with temporary in-memory keys.".to_owned(),
    ]
}

#[cfg(feature = "hardware")]
pub(super) fn render_hardware_profile_stepper(
    device_kind: HardwareDeviceKind,
    steps: &[HardwareProfileStepState],
) -> gpui::Div {
    let mut stepper = crate::root::app_stepper_container().w_full();
    let last_index = steps.len().saturating_sub(1);
    for (index, step) in steps.iter().enumerate() {
        stepper = stepper.child(render_hardware_profile_step(
            device_kind,
            step,
            index == last_index,
        ));
    }
    stepper
}

#[cfg(feature = "hardware")]
pub(super) fn hardware_profile_awaiting_approval(steps: &[HardwareProfileStepState]) -> bool {
    steps.iter().any(|step| {
        step.step == HardwareProfileStep::ApproveRailgunRequest
            && step.status == HardwareProfileStepStatus::Pending
    })
}

pub(super) fn hardware_wallet_setup_button_label(
    device_kind: HardwareDeviceKind,
    label: &'static str,
) -> gpui::Div {
    setup_button_label(hardware_device_symbol(device_kind), label)
}

pub(super) fn hardware_device_symbol(device_kind: HardwareDeviceKind) -> gpui::AnyElement {
    match device_kind {
        HardwareDeviceKind::Ledger => img(LEDGER_LOGO_SHORT_WHITE_ICON_PATH)
            .h(px(19.0))
            .flex_none()
            .into_any_element(),
        HardwareDeviceKind::Trezor => img(TREZOR_SYMBOL_WHITE_ICON_PATH)
            .h(px(22.0))
            .flex_none()
            .into_any_element(),
    }
}

#[cfg(feature = "hardware")]
pub(super) fn render_hardware_profile_approval_wait(
    device_kind: HardwareDeviceKind,
    approval_prompt: Option<&HardwareProfileApprovalPrompt>,
) -> gpui::Div {
    let device_label = hardware_device_label(device_kind);
    let copy = hardware_profile_approval_copy(device_kind, approval_prompt);
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb_with_alpha(theme::SURFACE, 0.72))
        .child(app_strong_text(format!("Confirm on {device_label}")))
        .child(app_muted_text(copy.intro).whitespace_normal())
        .children(copy.value.map(render_hardware_profile_approval_value))
        .child(app_muted_text(copy.warning).whitespace_normal())
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Spinner::new()
                        .icon(IconName::LoaderCircle)
                        .color(rgb(theme::SUCCESS).into())
                        .with_size(px(14.0)),
                )
                .child(
                    app_muted_shimmer_text(format!("Waiting for {device_label} approval..."))
                        .text_color(rgb(theme::SUCCESS)),
                ),
        )
}

#[cfg(feature = "hardware")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct HardwareProfileApprovalCopy {
    pub(in crate::root) intro: String,
    pub(in crate::root) value: Option<String>,
    pub(in crate::root) warning: String,
}

#[cfg(feature = "hardware")]
pub(in crate::root) fn hardware_profile_approval_copy(
    device_kind: HardwareDeviceKind,
    approval_prompt: Option<&HardwareProfileApprovalPrompt>,
) -> HardwareProfileApprovalCopy {
    let device_label = hardware_device_label(device_kind);
    match approval_prompt {
        Some(HardwareProfileApprovalPrompt::EvmAddress(address)) => HardwareProfileApprovalCopy {
            intro: format!(
                "Compare this {device_label} address with the one shown on your device:"
            ),
            value: Some(address.to_string()),
            warning: "Only approve if they match. Your device is asking to provide the Railgun public secret key."
                .to_owned(),
        },
        Some(HardwareProfileApprovalPrompt::TrezorCipherKeyValue(key_label)) => {
            HardwareProfileApprovalCopy {
                intro: "Your Trezor should show ENCRYPT VALUE for:".to_owned(),
                value: Some(key_label.to_string()),
                warning: "Only approve if this value matches. Your device is asking to provide the Railgun public secret key."
                    .to_owned(),
            }
        }
        None => HardwareProfileApprovalCopy {
            intro: format!(
                "Your {device_label} is asking to provide the Railgun public secret key."
            ),
            value: None,
            warning: "Review the request on the device, then approve it to continue.".to_owned(),
        },
    }
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_approval_value(value: String) -> gpui::Div {
    div()
        .w_full()
        .p(px(10.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .child(
            app_muted_text(value)
                .font_family(APP_MONO_FONT_FAMILY)
                .text_size(APP_TEXT_SIZE)
                .whitespace_normal(),
        )
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_step(
    device_kind: HardwareDeviceKind,
    step: &HardwareProfileStepState,
    is_last: bool,
) -> gpui::Div {
    let color = hardware_profile_step_color(step.status);
    let body = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .pb(if is_last { px(0.0) } else { px(12.0) })
        .child(
            app_strong_text(hardware_profile_step_label(device_kind, step.step))
                .text_color(rgb(color))
                .line_height(gpui::relative(1.0)),
        )
        .children(
            hardware_profile_step_detail(device_kind, step).map(|detail| {
                app_muted_text(detail)
                    .text_color(rgb(color))
                    .line_height(gpui::relative(1.0))
                    .whitespace_normal()
                    .into_any_element()
            }),
        );

    crate::root::app_step_row(
        render_hardware_profile_step_marker(step.status, color),
        body,
        is_last,
        color,
        px(30.0),
        Some(0.34),
    )
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_step_marker(status: HardwareProfileStepStatus, color: u32) -> gpui::Div {
    div()
        .size(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(rgb(color))
        .bg(rgb(theme::SURFACE))
        .text_color(rgb(color))
        .child(match status {
            HardwareProfileStepStatus::NotStarted => div()
                .size(px(7.0))
                .rounded_full()
                .bg(rgb(color))
                .into_any_element(),
            HardwareProfileStepStatus::Pending => Spinner::new()
                .icon(IconName::LoaderCircle)
                .color(rgb(color).into())
                .with_size(px(14.0))
                .into_any_element(),
            HardwareProfileStepStatus::Done => {
                Icon::new(IconName::CircleCheck).small().into_any_element()
            }
            HardwareProfileStepStatus::Error => Icon::new(IconName::TriangleAlert)
                .small()
                .into_any_element(),
        })
}

#[cfg(feature = "hardware")]
const fn hardware_profile_step_color(status: HardwareProfileStepStatus) -> u32 {
    match status {
        HardwareProfileStepStatus::NotStarted => theme::TEXT_MUTED,
        HardwareProfileStepStatus::Pending => theme::WARNING,
        HardwareProfileStepStatus::Done => theme::SUCCESS,
        HardwareProfileStepStatus::Error => theme::DANGER,
    }
}

#[cfg(feature = "hardware")]
fn hardware_profile_step_label(
    device_kind: HardwareDeviceKind,
    step: HardwareProfileStep,
) -> String {
    let device_label = hardware_device_label(device_kind);
    match step {
        HardwareProfileStep::UnlockDevice => format!("Unlock {device_label}"),
        HardwareProfileStep::OpenEthereumApp => match device_kind {
            HardwareDeviceKind::Ledger => "Open Ethereum app".to_owned(),
            HardwareDeviceKind::Trezor => "Confirm active wallet".to_owned(),
        },
        HardwareProfileStep::ApproveRailgunRequest => "Approve Railgun request".to_owned(),
    }
}

#[cfg(feature = "hardware")]
fn hardware_profile_step_detail(
    device_kind: HardwareDeviceKind,
    step: &HardwareProfileStepState,
) -> Option<String> {
    if matches!(
        step.status,
        HardwareProfileStepStatus::NotStarted | HardwareProfileStepStatus::Done
    ) {
        return None;
    }
    if let Some(message) = step.message.as_ref() {
        return Some(message.to_string());
    }
    if step.status == HardwareProfileStepStatus::Error {
        return Some("Needs attention.".to_owned());
    }
    let device_label = hardware_device_label(device_kind);
    Some(match (step.step, device_kind) {
        (HardwareProfileStep::UnlockDevice, _) => "Plug it in and enter your PIN.".to_owned(),
        (HardwareProfileStep::OpenEthereumApp, HardwareDeviceKind::Ledger) => {
            "Open the Ethereum app on your Ledger.".to_owned()
        }
        (HardwareProfileStep::OpenEthereumApp, HardwareDeviceKind::Trezor) => {
            "Confirm the wallet on your Trezor.".to_owned()
        }
        (HardwareProfileStep::ApproveRailgunRequest, _) => {
            format!("Approve the Railgun request on your {device_label}.")
        }
    })
}

#[cfg(feature = "hardware")]
pub(super) fn hardware_profile_error(message: &str) -> gpui::Div {
    div()
        .w_full()
        .child(Alert::error("wallet-hardware-profile-error", message.to_owned()).small())
}

#[cfg(feature = "hardware")]
pub(super) fn render_hardware_profile_account_row(
    root: &Entity<WalletRoot>,
    row: &crate::root::vault::HardwareAccountPickerRow,
    locked: bool,
) -> gpui::AnyElement {
    let wallet_id = Arc::clone(&row.wallet_id);
    let wallet_id_for_element = Arc::clone(&row.wallet_id);
    let supported = row.supported;
    let active = row.active;
    let label = row.label.to_string();
    let action_root = root.clone();
    let row_element = div()
        .w_full()
        .p(px(12.0))
        .flex()
        .items_center()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(if active && !locked {
            theme::PRIMARY
        } else if locked {
            theme::BORDER
        } else {
            theme::BORDER_STRONG
        }))
        .bg(rgb(if locked {
            theme::SURFACE
        } else {
            theme::SURFACE_ELEVATED
        }))
        .child(app_strong_text(label).truncate())
        .when(locked, |this| {
            this.child(
                app_muted_text("Locked")
                    .text_size(px(11.0))
                    .ml_auto()
                    .flex_none(),
            )
        })
        .when(!locked && !supported, |this| {
            this.child(
                app_muted_text("Unsupported")
                    .text_size(px(11.0))
                    .ml_auto()
                    .flex_none(),
            )
        });

    if locked || !supported {
        return row_element.into_any_element();
    }

    row_element
        .id(SharedString::from(format!(
            "hardware-profile-open-{wallet_id_for_element}"
        )))
        .cursor_pointer()
        .hover(|this| {
            this.bg(rgb(theme::SURFACE_HOVER))
                .border_color(rgb(theme::PRIMARY))
        })
        .on_click(move |_event, window, cx| {
            let wallet_id = Arc::clone(&wallet_id);
            action_root.update(cx, |root, cx| {
                root.open_hardware_account_from_profile_picker(wallet_id.as_ref(), window, cx);
            });
        })
        .into_any_element()
}

#[cfg(feature = "hardware")]
pub(in crate::root) fn render_trezor_pin_matrix_prompt(
    root: &Entity<WalletRoot>,
    prompt: &crate::root::vault::TrezorPinMatrixPromptState,
) -> gpui::Div {
    let submit_root = root.clone();
    let clear_root = root.clone();
    let backspace_root = root.clone();
    let positions_len = prompt.positions.len();
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb_with_alpha(theme::SURFACE, 0.72))
        .child(app_strong_text(trezor_pin_matrix_title(prompt.kind)))
        .child(
            app_muted_text(
                "Look at the randomized PIN digits on your Trezor and click the matching positions below. The app sends positions, not the digits shown on your device.",
            )
            .whitespace_normal(),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(render_trezor_pin_matrix_row(root, [7, 8, 9]))
                .child(render_trezor_pin_matrix_row(root, [4, 5, 6]))
                .child(render_trezor_pin_matrix_row(root, [1, 2, 3])),
        )
        .child(app_muted_text(format!(
            "Positions entered: {}",
            "•".repeat(positions_len)
        )))
        .child(
            div()
                .w_full()
                .flex()
                .gap_2()
                .child(
                    app_button("trezor-pin-clear", "Clear")
                        .outline()
                        .flex_1()
                        .disabled(positions_len == 0)
                        .on_click(move |_event, _window, cx| {
                            clear_root.update(cx, |root, cx| {
                                root.clear_trezor_pin_matrix_positions(cx);
                            });
                        }),
                )
                .child(
                    app_button("trezor-pin-backspace", "Backspace")
                        .outline()
                        .flex_1()
                        .disabled(positions_len == 0)
                        .on_click(move |_event, _window, cx| {
                            backspace_root.update(cx, |root, cx| {
                                root.backspace_trezor_pin_matrix_position(cx);
                            });
                        }),
                )
                .child(
                    app_button("trezor-pin-submit", "Submit PIN")
                        .primary()
                        .flex_1()
                        .disabled(positions_len == 0)
                        .on_click(move |_event, _window, cx| {
                            submit_root.update(cx, |root, cx| {
                                root.submit_trezor_pin_matrix_positions(cx);
                            });
                        }),
                ),
        )
}

#[cfg(feature = "hardware")]
fn render_trezor_pin_matrix_row(root: &Entity<WalletRoot>, positions: [u8; 3]) -> gpui::Div {
    positions
        .into_iter()
        .fold(div().w_full().flex().gap_2(), |row, position| {
            row.child(render_trezor_pin_matrix_position_button(root, position))
        })
}

#[cfg(feature = "hardware")]
fn render_trezor_pin_matrix_position_button(root: &Entity<WalletRoot>, position: u8) -> Button {
    let position_root = root.clone();
    app_button_base(("trezor-pin-position", usize::from(position)))
        .outline()
        .flex_1()
        .min_w(px(0.0))
        .h(px(44.0))
        .child(app_button_label(" "))
        .on_click(move |_event, _window, cx| {
            let position = char::from(b'0' + position);
            position_root.update(cx, |root, cx| {
                root.push_trezor_pin_matrix_position(position, cx);
            });
        })
}

#[cfg(feature = "hardware")]
pub(super) fn trezor_passphrase_mode_segment_button(
    id: SharedString,
    label: &'static str,
    selected: bool,
    disabled: bool,
) -> Button {
    let button = app_button_base(id)
        .tab_stop(false)
        .flex_1()
        .min_w(px(0.0))
        .selected(selected)
        .disabled(disabled)
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .child(app_button_label(label)),
        );
    if selected { button.primary() } else { button }
}
