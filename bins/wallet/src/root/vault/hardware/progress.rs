#[cfg(feature = "hardware")]
use super::{
    Arc, Duration, HardwareDerivationError, HardwareDeviceKind, HardwareProfileApprovalPrompt,
    HardwareProfilePinMatrixRequest, HardwareProfileProgressUpdate, HardwareProfileStep,
    HardwareProfileStepStatus, TrezorPinMatrixProvider, TrezorPinMatrixRequestKind, Zeroizing,
    hardware_device_label, mpsc,
};
#[cfg(feature = "hardware")]
use wallet_ops::hardware::TREZOR_USB_UNAVAILABLE_GUIDANCE;

#[cfg(feature = "hardware")]
pub(super) const HARDWARE_PROFILE_READINESS_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(feature = "hardware")]
pub(in crate::root) const fn hardware_setup_error_preserves_password(
    error: &HardwareDerivationError,
) -> bool {
    error.is_early_device_readiness_error()
}

#[cfg(feature = "hardware")]
pub(in crate::root) fn hardware_profile_hardware_error_message(
    operation: &str,
    error: &HardwareDerivationError,
    awaiting_approval: bool,
) -> Arc<str> {
    let awaiting_approval = awaiting_approval
        || matches!(
            error,
            HardwareDerivationError::LedgerStatus {
                operation: "derive Railgun secret",
                ..
            }
        );
    if error.is_ledger_busy_error() {
        return "Ledger connection is busy. Make sure no other wallet app is using it, keep the Ethereum app open, then try again. If this keeps happening, unplug and reconnect your Ledger."
            .into();
    }
    if matches!(error, HardwareDerivationError::TrezorUsbUnavailable(_)) {
        return TREZOR_USB_UNAVAILABLE_GUIDANCE.into();
    }
    if awaiting_approval {
        match error {
            HardwareDerivationError::LedgerStatus { status: 0x6982, .. } => {
                return "Request rejected on Ledger. Try again when you are ready to approve it."
                    .into();
            }
            HardwareDerivationError::LedgerStatus { status: 0x6985, .. } => {
                return "Ledger did not approve the request. If it locked or timed out, enter your PIN, then open the Ethereum app and try again."
                    .into();
            }
            HardwareDerivationError::LedgerStatus {
                status: 0x6804 | 0x6b0c,
                ..
            } => {
                return "Ledger locked before the request was approved. Enter your PIN, then open the Ethereum app and try again."
                    .into();
            }
            HardwareDerivationError::LedgerStatus {
                status: 0x6511 | 0x6a15 | 0x6d00 | 0x6e00,
                ..
            } => {
                return "The Ethereum app closed before the request was approved. Enter your PIN, then open the Ethereum app and try again."
                    .into();
            }
            HardwareDerivationError::LedgerUnavailable(_) | HardwareDerivationError::Ledger(_) => {
                return "Ledger locked or disconnected before the request was approved. Plug it in and enter your PIN, then open the Ethereum app and try again."
                    .into();
            }
            HardwareDerivationError::TrezorLocked
            | HardwareDerivationError::UnsupportedTrezorPinMatrix => {
                return "Trezor locked before the request was approved. Enter your PIN, then try again."
                    .into();
            }
            HardwareDerivationError::Trezor(_) => {
                return "Trezor locked or disconnected before the request was approved. Plug it in and enter your PIN, then try again."
                    .into();
            }
            _ => {}
        }
    }

    match error {
        HardwareDerivationError::LedgerUnavailable(_) => {
            "Plug it in and enter your PIN, then open the Ethereum app and try again.".into()
        }
        HardwareDerivationError::LedgerStatus { message, .. } => (*message).into(),
        HardwareDerivationError::Ledger(_) => {
            "Ledger communication failed. Keep the Ethereum app open, then try again. If this keeps happening, unplug and reconnect your Ledger."
                .into()
        }
        HardwareDerivationError::Trezor(_) => {
            "Plug it in and enter your PIN, then try again.".into()
        }
        HardwareDerivationError::TrezorLocked
        | HardwareDerivationError::UnsupportedTrezorPinMatrix => {
            "Enter your PIN, then try again.".into()
        }
        _ => format!("{operation}: {error}").into(),
    }
}

#[cfg(feature = "hardware")]
pub(in crate::root) const fn hardware_profile_should_reconnect_after_error(
    error: &HardwareDerivationError,
    awaiting_approval: bool,
) -> bool {
    if awaiting_approval || error.is_ledger_busy_error() {
        return false;
    }
    match error {
        HardwareDerivationError::LedgerUnavailable(_) | HardwareDerivationError::Ledger(_) => true,
        HardwareDerivationError::LedgerStatus { status, .. } => {
            ledger_locked_status(*status) || ledger_ethereum_app_status(*status)
        }
        _ => false,
    }
}

#[cfg(feature = "hardware")]
pub(in crate::root) fn hardware_profile_detection_should_retry(
    error: &HardwareDerivationError,
) -> bool {
    if error.is_early_device_readiness_error() {
        return true;
    }
    matches!(
        error,
        HardwareDerivationError::LedgerStatus {
            operation: "get Ethereum app version" | "get Ethereum address",
            ..
        }
    )
}

#[cfg(feature = "hardware")]
pub(in crate::root) const fn hardware_profile_detection_should_suppress_initial_trezor_progress(
    error: &HardwareDerivationError,
) -> bool {
    matches!(
        error,
        HardwareDerivationError::TrezorLocked
            | HardwareDerivationError::UnsupportedTrezorPinMatrix
            | HardwareDerivationError::TrezorUsbUnavailable(_)
    )
}

#[cfg(all(test, feature = "hardware"))]
pub(in crate::root) fn hardware_profile_detection_ledger_is_unlocked(
    error: &HardwareDerivationError,
) -> bool {
    matches!(
        error,
        HardwareDerivationError::LedgerStatus {
            operation: "get Ethereum app version" | "get Ethereum address",
            status,
            ..
        } if ledger_ethereum_app_status(*status)
    )
}

#[cfg(feature = "hardware")]
pub(in crate::root) const fn hardware_profile_detection_should_suppress_initial_ledger_progress(
    error: &HardwareDerivationError,
) -> bool {
    matches!(
        error,
        HardwareDerivationError::LedgerStatus { .. } | HardwareDerivationError::Ledger(_)
    )
}

#[cfg(feature = "hardware")]
pub(super) fn send_hardware_profile_progress(
    progress_tx: &mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
    step: HardwareProfileStep,
    status: HardwareProfileStepStatus,
    message: Option<&str>,
) -> bool {
    progress_tx
        .send(HardwareProfileProgressUpdate {
            step,
            status,
            message: message.map(ToOwned::to_owned),
            apply_step: true,
            trezor_passphrase_always_on_device: None,
            approval_prompt: None,
            trezor_pin_matrix_request: None,
            clear_trezor_pin_matrix_request: false,
        })
        .is_ok()
}

#[cfg(feature = "hardware")]
pub(super) fn send_trezor_passphrase_policy_progress(
    progress_tx: &mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
    passphrase_always_on_device: bool,
) -> bool {
    progress_tx
        .send(HardwareProfileProgressUpdate {
            step: HardwareProfileStep::UnlockDevice,
            status: HardwareProfileStepStatus::Pending,
            message: None,
            apply_step: false,
            trezor_passphrase_always_on_device: Some(passphrase_always_on_device),
            approval_prompt: None,
            trezor_pin_matrix_request: None,
            clear_trezor_pin_matrix_request: false,
        })
        .is_ok()
}

#[cfg(feature = "hardware")]
fn send_trezor_pin_matrix_prompt_progress(
    progress_tx: &mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
    kind: TrezorPinMatrixRequestKind,
    response_tx: std::sync::mpsc::Sender<Zeroizing<String>>,
) -> bool {
    progress_tx
        .send(HardwareProfileProgressUpdate {
            step: HardwareProfileStep::UnlockDevice,
            status: HardwareProfileStepStatus::Pending,
            message: Some("Enter your Trezor PIN using the matrix below.".to_owned()),
            apply_step: true,
            trezor_passphrase_always_on_device: None,
            approval_prompt: None,
            trezor_pin_matrix_request: Some(HardwareProfilePinMatrixRequest { kind, response_tx }),
            clear_trezor_pin_matrix_request: false,
        })
        .is_ok()
}

#[cfg(feature = "hardware")]
fn send_trezor_pin_matrix_clear_progress(
    progress_tx: &mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
) -> bool {
    progress_tx
        .send(HardwareProfileProgressUpdate {
            step: HardwareProfileStep::UnlockDevice,
            status: HardwareProfileStepStatus::Pending,
            message: None,
            apply_step: false,
            trezor_passphrase_always_on_device: None,
            approval_prompt: None,
            trezor_pin_matrix_request: None,
            clear_trezor_pin_matrix_request: true,
        })
        .is_ok()
}

#[cfg(feature = "hardware")]
pub(super) fn trezor_pin_matrix_provider(
    progress_tx: mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
) -> TrezorPinMatrixProvider {
    Arc::new(move |kind| {
        let (response_tx, response_rx) = std::sync::mpsc::channel();
        if !send_trezor_pin_matrix_prompt_progress(&progress_tx, kind, response_tx) {
            return Err(HardwareDerivationError::TrezorPinEntryCancelled);
        }
        let pin = response_rx
            .recv()
            .map_err(|_| HardwareDerivationError::TrezorPinEntryCancelled)?;
        let _ = send_trezor_pin_matrix_clear_progress(&progress_tx);
        let _ = send_hardware_profile_progress(
            &progress_tx,
            HardwareProfileStep::UnlockDevice,
            HardwareProfileStepStatus::Done,
            None,
        );
        let _ = send_hardware_profile_progress(
            &progress_tx,
            HardwareProfileStep::OpenEthereumApp,
            HardwareProfileStepStatus::Pending,
            Some("Confirm the wallet on your Trezor."),
        );
        Ok(pin)
    })
}

#[cfg(feature = "hardware")]
pub(super) fn send_hardware_profile_approval_progress(
    device_kind: HardwareDeviceKind,
    approval_prompt: Option<HardwareProfileApprovalPrompt>,
    progress_tx: &mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
) -> bool {
    let device_label = hardware_device_label(device_kind);
    let message = format!("Approve the Railgun request on your {device_label}.");
    send_hardware_profile_progress(
        progress_tx,
        HardwareProfileStep::UnlockDevice,
        HardwareProfileStepStatus::Done,
        None,
    ) && send_hardware_profile_progress(
        progress_tx,
        HardwareProfileStep::OpenEthereumApp,
        HardwareProfileStepStatus::Done,
        None,
    ) && progress_tx
        .send(HardwareProfileProgressUpdate {
            step: HardwareProfileStep::ApproveRailgunRequest,
            status: HardwareProfileStepStatus::Pending,
            message: Some(message),
            apply_step: true,
            trezor_passphrase_always_on_device: None,
            approval_prompt,
            trezor_pin_matrix_request: None,
            clear_trezor_pin_matrix_request: false,
        })
        .is_ok()
}

#[cfg(feature = "hardware")]
pub(super) fn send_hardware_profile_readiness_progress(
    device_kind: HardwareDeviceKind,
    error: &HardwareDerivationError,
    progress_tx: &mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
) -> bool {
    match device_kind {
        HardwareDeviceKind::Ledger => match error {
            HardwareDerivationError::LedgerStatus { status, .. }
                if ledger_locked_status(*status) =>
            {
                send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::UnlockDevice,
                    HardwareProfileStepStatus::Pending,
                    Some("Enter your PIN."),
                ) && send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::OpenEthereumApp,
                    HardwareProfileStepStatus::NotStarted,
                    None,
                )
            }
            HardwareDerivationError::LedgerStatus { status, .. }
                if ledger_ethereum_app_status(*status) =>
            {
                send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::UnlockDevice,
                    HardwareProfileStepStatus::Done,
                    None,
                ) && send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::OpenEthereumApp,
                    HardwareProfileStepStatus::Pending,
                    Some("Open the Ethereum app on your Ledger."),
                )
            }
            HardwareDerivationError::LedgerUnavailable(_) => {
                send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::UnlockDevice,
                    HardwareProfileStepStatus::Pending,
                    Some("Plug it in and enter your PIN."),
                ) && send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::OpenEthereumApp,
                    HardwareProfileStepStatus::NotStarted,
                    None,
                ) && send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::ApproveRailgunRequest,
                    HardwareProfileStepStatus::NotStarted,
                    None,
                )
            }
            _ => {
                send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::UnlockDevice,
                    HardwareProfileStepStatus::Pending,
                    Some("Enter your PIN, then open the Ethereum app."),
                ) && send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::OpenEthereumApp,
                    HardwareProfileStepStatus::NotStarted,
                    None,
                )
            }
        },
        HardwareDeviceKind::Trezor => {
            let unlock_message = match error {
                HardwareDerivationError::TrezorLocked
                | HardwareDerivationError::UnsupportedTrezorPinMatrix => "Enter your PIN.",
                HardwareDerivationError::TrezorUsbUnavailable(_) => TREZOR_USB_UNAVAILABLE_GUIDANCE,
                _ => "Plug it in and enter your PIN.",
            };
            send_hardware_profile_progress(
                progress_tx,
                HardwareProfileStep::UnlockDevice,
                HardwareProfileStepStatus::Pending,
                Some(unlock_message),
            ) && send_hardware_profile_progress(
                progress_tx,
                HardwareProfileStep::OpenEthereumApp,
                HardwareProfileStepStatus::NotStarted,
                None,
            ) && send_hardware_profile_progress(
                progress_tx,
                HardwareProfileStep::ApproveRailgunRequest,
                HardwareProfileStepStatus::NotStarted,
                None,
            )
        }
    }
}

#[cfg(feature = "hardware")]
const fn ledger_locked_status(status: u16) -> bool {
    matches!(status, 0x6804 | 0x6b0c)
}

#[cfg(feature = "hardware")]
const fn ledger_ethereum_app_status(status: u16) -> bool {
    matches!(status, 0x6511 | 0x6a15 | 0x6d00 | 0x6e00)
}

#[cfg(all(test, feature = "hardware"))]
mod hardware_profile_detection_tests {
    use super::*;

    #[test]
    fn generic_ledger_detection_status_keeps_unlock_pending() {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let error = HardwareDerivationError::LedgerStatus {
            operation: "get Ethereum address",
            status: 0x5515,
            message: "Ledger returned an unexpected status. Open the Ethereum app on your Ledger and retry.",
        };

        assert!(send_hardware_profile_readiness_progress(
            HardwareDeviceKind::Ledger,
            &error,
            &progress_tx,
        ));

        let unlock = progress_rx.try_recv().expect("unlock progress update");
        assert_eq!(unlock.step, HardwareProfileStep::UnlockDevice);
        assert_eq!(unlock.status, HardwareProfileStepStatus::Pending);
        assert_eq!(
            unlock.message.as_deref(),
            Some("Enter your PIN, then open the Ethereum app.")
        );

        let ethereum = progress_rx
            .try_recv()
            .expect("ethereum app progress update");
        assert_eq!(ethereum.step, HardwareProfileStep::OpenEthereumApp);
        assert_eq!(ethereum.status, HardwareProfileStepStatus::NotStarted);
        assert_eq!(ethereum.message, None);
        assert!(progress_rx.try_recv().is_err());
    }

    #[test]
    fn ethereum_app_ledger_detection_status_keeps_open_ethereum_pending() {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let error = HardwareDerivationError::LedgerStatus {
            operation: "get Ethereum address",
            status: 0x6511,
            message: "Open the Ethereum app on your Ledger, then retry.",
        };

        assert!(send_hardware_profile_readiness_progress(
            HardwareDeviceKind::Ledger,
            &error,
            &progress_tx,
        ));

        let unlock = progress_rx.try_recv().expect("unlock progress update");
        assert_eq!(unlock.step, HardwareProfileStep::UnlockDevice);
        assert_eq!(unlock.status, HardwareProfileStepStatus::Done);
        assert_eq!(unlock.message, None);

        let ethereum = progress_rx
            .try_recv()
            .expect("ethereum app progress update");
        assert_eq!(ethereum.step, HardwareProfileStep::OpenEthereumApp);
        assert_eq!(ethereum.status, HardwareProfileStepStatus::Pending);
        assert_eq!(
            ethereum.message.as_deref(),
            Some("Open the Ethereum app on your Ledger.")
        );
        assert!(progress_rx.try_recv().is_err());
    }

    #[test]
    fn ledger_unavailable_resets_downstream_progress() {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let ethereum_app_error = HardwareDerivationError::LedgerStatus {
            operation: "get Ethereum address",
            status: 0x6511,
            message: "Open the Ethereum app on your Ledger, then retry.",
        };
        assert!(send_hardware_profile_readiness_progress(
            HardwareDeviceKind::Ledger,
            &ethereum_app_error,
            &progress_tx,
        ));
        progress_rx.try_recv().expect("unlock done update");
        let ethereum = progress_rx.try_recv().expect("ethereum app pending update");
        assert_eq!(ethereum.step, HardwareProfileStep::OpenEthereumApp);
        assert_eq!(ethereum.status, HardwareProfileStepStatus::Pending);

        let unavailable = HardwareDerivationError::LedgerUnavailable("Ledger is not connected");
        assert!(send_hardware_profile_readiness_progress(
            HardwareDeviceKind::Ledger,
            &unavailable,
            &progress_tx,
        ));

        let unlock = progress_rx.try_recv().expect("unlock pending update");
        assert_eq!(unlock.step, HardwareProfileStep::UnlockDevice);
        assert_eq!(unlock.status, HardwareProfileStepStatus::Pending);
        assert_eq!(
            unlock.message.as_deref(),
            Some("Plug it in and enter your PIN.")
        );

        let ethereum = progress_rx.try_recv().expect("ethereum app reset update");
        assert_eq!(ethereum.step, HardwareProfileStep::OpenEthereumApp);
        assert_eq!(ethereum.status, HardwareProfileStepStatus::NotStarted);
        assert_eq!(ethereum.message, None);

        let approval = progress_rx.try_recv().expect("approval reset update");
        assert_eq!(approval.step, HardwareProfileStep::ApproveRailgunRequest);
        assert_eq!(approval.status, HardwareProfileStepStatus::NotStarted);
        assert_eq!(approval.message, None);
        assert!(progress_rx.try_recv().is_err());
    }

    #[test]
    fn trezor_locked_resets_downstream_progress() {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let error = HardwareDerivationError::TrezorLocked;

        assert!(send_hardware_profile_readiness_progress(
            HardwareDeviceKind::Trezor,
            &error,
            &progress_tx,
        ));

        let unlock = progress_rx.try_recv().expect("unlock pending update");
        assert_eq!(unlock.step, HardwareProfileStep::UnlockDevice);
        assert_eq!(unlock.status, HardwareProfileStepStatus::Pending);
        assert_eq!(unlock.message.as_deref(), Some("Enter your PIN."));

        let context = progress_rx.try_recv().expect("context reset update");
        assert_eq!(context.step, HardwareProfileStep::OpenEthereumApp);
        assert_eq!(context.status, HardwareProfileStepStatus::NotStarted);
        assert_eq!(context.message, None);

        let approval = progress_rx.try_recv().expect("approval reset update");
        assert_eq!(approval.step, HardwareProfileStep::ApproveRailgunRequest);
        assert_eq!(approval.status, HardwareProfileStepStatus::NotStarted);
        assert_eq!(approval.message, None);
        assert!(progress_rx.try_recv().is_err());
    }

    #[test]
    fn trezor_usb_unavailable_keeps_guidance_across_retries() {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let error = HardwareDerivationError::TrezorUsbUnavailable("busy".to_owned());

        assert!(hardware_profile_detection_should_retry(&error));
        assert!(hardware_profile_detection_should_suppress_initial_trezor_progress(&error));
        assert!(send_hardware_profile_readiness_progress(
            HardwareDeviceKind::Trezor,
            &error,
            &progress_tx,
        ));

        let unlock = progress_rx.try_recv().expect("unlock pending update");
        assert_eq!(unlock.step, HardwareProfileStep::UnlockDevice);
        assert_eq!(unlock.status, HardwareProfileStepStatus::Pending);
        assert_eq!(
            unlock.message.as_deref(),
            Some(TREZOR_USB_UNAVAILABLE_GUIDANCE)
        );
    }
}
