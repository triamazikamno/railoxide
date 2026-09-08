#[cfg(feature = "hardware")]
use super::{
    Arc, DesktopViewSession, HardwareDerivationError, HardwareDeviceKind, HardwareProfileMetadata,
    HardwareProfileSession, HardwareRailgunAccountMetadata, TrezorPassphraseMode,
    TrezorPinMatrixRequestKind, VaultError, ViewUnlock, WalletMetadataBundle, Zeroize, Zeroizing,
};

#[cfg(feature = "hardware")]
pub(super) enum HardwareWalletCreationError {
    Hardware {
        error: HardwareDerivationError,
        awaiting_approval: bool,
    },
    Vault(VaultError),
}

#[cfg(feature = "hardware")]
pub(super) type HardwareWalletCreationResult = (DesktopViewSession, Vec<WalletMetadataBundle>);

#[cfg(feature = "hardware")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum HardwareProfileStep {
    UnlockDevice,
    OpenEthereumApp,
    ApproveRailgunRequest,
}

#[cfg(feature = "hardware")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum HardwareProfileStepStatus {
    NotStarted,
    Pending,
    Done,
    Error,
}

#[cfg(feature = "hardware")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum HardwareProfilePickerView {
    Summary,
    ChooseDefaultSyncIntent,
}

#[cfg(feature = "hardware")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum HardwareProfileUnlockPurpose {
    Open,
    Add,
    Delete,
}

#[cfg(feature = "hardware")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct HardwareProfileStepState {
    pub(in crate::root) step: HardwareProfileStep,
    pub(in crate::root) status: HardwareProfileStepStatus,
    pub(in crate::root) message: Option<Arc<str>>,
}

#[cfg(feature = "hardware")]
pub(super) struct HardwareProfileProgressUpdate {
    pub(super) step: HardwareProfileStep,
    pub(super) status: HardwareProfileStepStatus,
    pub(super) message: Option<String>,
    pub(super) apply_step: bool,
    pub(super) trezor_passphrase_always_on_device: Option<bool>,
    pub(super) approval_prompt: Option<HardwareProfileApprovalPrompt>,
    pub(super) trezor_pin_matrix_request: Option<HardwareProfilePinMatrixRequest>,
    pub(super) clear_trezor_pin_matrix_request: bool,
}

#[cfg(feature = "hardware")]
pub(super) struct HardwareProfilePinMatrixRequest {
    pub(super) kind: TrezorPinMatrixRequestKind,
    pub(super) response_tx: std::sync::mpsc::Sender<Zeroizing<String>>,
}

#[cfg(feature = "hardware")]
pub(in crate::root) struct TrezorPinMatrixPromptState {
    pub(in crate::root) kind: TrezorPinMatrixRequestKind,
    pub(in crate::root) positions: String,
    pub(super) response_tx: Option<std::sync::mpsc::Sender<Zeroizing<String>>>,
}

#[cfg(feature = "hardware")]
impl TrezorPinMatrixPromptState {
    pub(super) fn clear_sensitive(&mut self) {
        self.positions.zeroize();
    }
}

#[cfg(feature = "hardware")]
#[derive(Clone)]
pub(in crate::root) struct HardwareAccountPickerRow {
    pub(in crate::root) wallet_id: Arc<str>,
    pub(in crate::root) label: Arc<str>,
    pub(in crate::root) account_index: u32,
    pub(in crate::root) account: HardwareRailgunAccountMetadata,
    pub(in crate::root) supported: bool,
    pub(in crate::root) active: bool,
}

#[cfg(feature = "hardware")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) enum HardwareProfileApprovalPrompt {
    EvmAddress(Arc<str>),
    TrezorCipherKeyValue(Arc<str>),
}

#[cfg(feature = "hardware")]
pub(in crate::root) struct HardwareProfileUnlockState {
    pub(in crate::root) target_wallet_id: Option<Arc<str>>,
    pub(in crate::root) purpose: HardwareProfileUnlockPurpose,
    pub(in crate::root) device_kind: Option<HardwareDeviceKind>,
    pub(in crate::root) session: Option<HardwareProfileSession>,
    pub(in crate::root) profile: Option<HardwareProfileMetadata>,
    pub(in crate::root) accounts: Vec<HardwareAccountPickerRow>,
    pub(in crate::root) locked_accounts: Vec<HardwareAccountPickerRow>,
    pub(in crate::root) vault_view_unlock: Option<Arc<ViewUnlock>>,
    pub(in crate::root) in_progress: bool,
    pub(in crate::root) action_label: Option<Arc<str>>,
    pub(in crate::root) approval_prompt: Option<HardwareProfileApprovalPrompt>,
    pub(in crate::root) error: Option<Arc<str>>,
    pub(in crate::root) trezor_passphrase_mode: TrezorPassphraseMode,
    pub(in crate::root) trezor_passphrase_always_on_device: Option<bool>,
    pub(in crate::root) trezor_pin_matrix_prompt: Option<TrezorPinMatrixPromptState>,
    pub(in crate::root) progress_steps: Vec<HardwareProfileStepState>,
    pub(in crate::root) picker_view: HardwareProfilePickerView,
    pub(in crate::root) advanced_open: bool,
    pub(super) reconnect_notice: Option<Arc<str>>,
}

#[cfg(feature = "hardware")]
impl Default for HardwareProfileUnlockState {
    fn default() -> Self {
        Self {
            target_wallet_id: None,
            purpose: HardwareProfileUnlockPurpose::Open,
            device_kind: None,
            session: None,
            profile: None,
            accounts: Vec::new(),
            locked_accounts: Vec::new(),
            vault_view_unlock: None,
            in_progress: false,
            action_label: None,
            approval_prompt: None,
            error: None,
            trezor_passphrase_mode: TrezorPassphraseMode::NoPassphrase,
            trezor_passphrase_always_on_device: None,
            trezor_pin_matrix_prompt: None,
            progress_steps: default_hardware_profile_steps(),
            picker_view: HardwareProfilePickerView::Summary,
            advanced_open: false,
            reconnect_notice: None,
        }
    }
}

#[cfg(feature = "hardware")]
pub(in crate::root) fn dismiss_hardware_profile_unlock_state(
    state: &mut HardwareProfileUnlockState,
) {
    state.clear_sensitive();
    *state = HardwareProfileUnlockState::default();
}

#[cfg(feature = "hardware")]
impl HardwareProfileUnlockState {
    pub(super) fn clear_sensitive(&mut self) {
        self.vault_view_unlock = None;
        self.clear_trezor_pin_matrix_prompt();
    }

    pub(super) fn clear_trezor_pin_matrix_prompt(&mut self) {
        if let Some(mut prompt) = self.trezor_pin_matrix_prompt.take() {
            prompt.clear_sensitive();
        }
    }

    pub(super) fn reset_for_device(
        &mut self,
        device_kind: HardwareDeviceKind,
        target_wallet_id: Option<Arc<str>>,
        purpose: HardwareProfileUnlockPurpose,
    ) {
        self.clear_sensitive();
        self.target_wallet_id = target_wallet_id;
        self.purpose = purpose;
        self.device_kind = Some(device_kind);
        self.session = None;
        self.profile = None;
        self.accounts.clear();
        self.locked_accounts.clear();
        self.in_progress = false;
        self.action_label = None;
        self.approval_prompt = None;
        self.error = None;
        self.trezor_passphrase_mode = TrezorPassphraseMode::NoPassphrase;
        self.trezor_passphrase_always_on_device = None;
        self.clear_trezor_pin_matrix_prompt();
        self.progress_steps = default_hardware_profile_steps();
        self.picker_view = HardwareProfilePickerView::Summary;
        self.advanced_open = false;
        self.reconnect_notice = None;
    }

    pub(super) fn set_progress_step(
        &mut self,
        step: HardwareProfileStep,
        status: HardwareProfileStepStatus,
        message: Option<impl Into<Arc<str>>>,
    ) {
        if let Some(existing) = self
            .progress_steps
            .iter_mut()
            .find(|existing| existing.step == step)
        {
            existing.status = status;
            existing.message = message.map(Into::into);
        }
    }

    pub(super) fn mark_first_pending_progress_step_error(&mut self, message: impl Into<Arc<str>>) {
        let message = Some(message.into());
        if let Some(step) = self
            .progress_steps
            .iter_mut()
            .find(|step| step.status == HardwareProfileStepStatus::Pending)
        {
            step.status = HardwareProfileStepStatus::Error;
            step.message = message;
        }
    }

    #[must_use]
    pub(super) fn awaiting_approval(&self) -> bool {
        self.progress_steps.iter().any(|step| {
            step.step == HardwareProfileStep::ApproveRailgunRequest
                && step.status == HardwareProfileStepStatus::Pending
        })
    }
}

#[cfg(feature = "hardware")]
pub(super) fn default_hardware_profile_steps() -> Vec<HardwareProfileStepState> {
    [
        HardwareProfileStep::UnlockDevice,
        HardwareProfileStep::OpenEthereumApp,
        HardwareProfileStep::ApproveRailgunRequest,
    ]
    .into_iter()
    .map(|step| HardwareProfileStepState {
        step,
        status: HardwareProfileStepStatus::NotStarted,
        message: None,
    })
    .collect()
}

#[cfg(feature = "hardware")]
impl From<HardwareDerivationError> for HardwareWalletCreationError {
    fn from(error: HardwareDerivationError) -> Self {
        Self::Hardware {
            error,
            awaiting_approval: false,
        }
    }
}

#[cfg(feature = "hardware")]
pub(super) const fn hardware_approval_error(
    error: HardwareDerivationError,
) -> HardwareWalletCreationError {
    HardwareWalletCreationError::Hardware {
        error,
        awaiting_approval: true,
    }
}

#[cfg(feature = "hardware")]
impl From<VaultError> for HardwareWalletCreationError {
    fn from(error: VaultError) -> Self {
        Self::Vault(error)
    }
}
