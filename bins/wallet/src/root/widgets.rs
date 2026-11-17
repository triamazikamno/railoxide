//! The input and pane state `WalletRoot::new` creates before it assembles the root.

#[cfg(feature = "hardware")]
use gpui::FocusHandle;
use gpui::{AppContext, Context, Entity, Window};
use gpui_component::input::{InputState, TextareaState};
use gpui_component::select::SearchableVec;
use ui::logs::LogsPane;

use super::address_book::AddressBookState;
use super::governance::{self, ParticipantPickerState};
use super::logs_search::LogsSearchSource;
use super::public_account::PublicAccountFormState;
use super::search_field::SearchField;
use super::{WalletRoot, new_masked_input, new_text_input, wallet_activity};

pub(super) struct WalletRootWidgets {
    pub(super) unlock_password_input: Entity<InputState>,
    pub(super) new_password_input: Entity<InputState>,
    pub(super) confirm_password_input: Entity<InputState>,
    pub(super) wallet_name_input: Entity<InputState>,
    pub(super) manage_wallet_label_input: Entity<InputState>,
    pub(super) add_wallet_password_input: Entity<InputState>,
    pub(super) hardware_wallet_restore_account_index_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    pub(super) hardware_profile_password_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    pub(super) hardware_profile_label_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    pub(super) hardware_profile_recovery_start_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    pub(super) hardware_profile_recovery_count_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    pub(super) hardware_profile_exact_index_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    pub(super) trezor_app_passphrase_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    pub(super) trezor_passphrase_mode_focus: FocusHandle,
    pub(super) import_mnemonic_input: Entity<TextareaState>,
    pub(super) wallet_activity: wallet_activity::WalletActivityState,
    pub(super) logs_search: Entity<SearchField<LogsSearchSource>>,
    pub(super) governance_participant_picker: Entity<ParticipantPickerState>,
    pub(super) governance_proposal_action_amount_input: Entity<InputState>,
    pub(super) governance_staking_delegate_input: Entity<InputState>,
    pub(super) favorite_broadcaster_input: Entity<InputState>,
    pub(super) banned_broadcaster_input: Entity<InputState>,
    pub(super) address_book_label_input: Entity<InputState>,
    pub(super) address_book: AddressBookState,
    pub(super) public_form: PublicAccountFormState,
    pub(super) repair_cache_block_input: Entity<InputState>,
    pub(super) tx_search_input: Entity<InputState>,
}

impl WalletRootWidgets {
    pub(super) fn new(
        logs: &Entity<LogsPane>,
        mimic_railway_shields_by_default: bool,
        window: &mut Window,
        cx: &mut Context<'_, WalletRoot>,
    ) -> Self {
        let unlock_password_input = new_masked_input(window, cx, "vault password");
        let new_password_input = new_masked_input(window, cx, "new vault password");
        let confirm_password_input = new_masked_input(window, cx, "confirm vault password");
        let wallet_name_input = new_text_input(window, cx, "wallet name");
        let manage_wallet_label_input = new_text_input(window, cx, "wallet label");
        let add_wallet_password_input = new_masked_input(window, cx, "vault password");
        let hardware_wallet_restore_account_index_input =
            new_text_input(window, cx, "optional restore account index");
        #[cfg(feature = "hardware")]
        let hardware_profile_password_input = new_masked_input(window, cx, "vault password");
        #[cfg(feature = "hardware")]
        let hardware_profile_label_input = new_text_input(window, cx, "hardware profile label");
        #[cfg(feature = "hardware")]
        let hardware_profile_recovery_start_input = new_text_input(window, cx, "start index");
        #[cfg(feature = "hardware")]
        let hardware_profile_recovery_count_input = new_text_input(window, cx, "count");
        #[cfg(feature = "hardware")]
        let hardware_profile_exact_index_input = new_text_input(window, cx, "account index");
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase_input = new_masked_input(window, cx, "Trezor passphrase");
        #[cfg(feature = "hardware")]
        let trezor_passphrase_mode_focus = cx.focus_handle();
        let import_mnemonic_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 6)
                .placeholder("paste recovery phrase")
        });
        let public_account_search_input = new_text_input(window, cx, "Search accounts");
        let wallet_activity = wallet_activity::WalletActivityState::new(window, cx);
        let log_store = logs.read(cx).store().clone();
        // Debug UI fixture: the `logs` mode fills the buffer with representative lines.
        #[cfg(debug_assertions)]
        super::private_swap::ui_fixture_fill_logs(&log_store);
        let logs_search =
            cx.new(|cx| SearchField::new(LogsSearchSource { store: log_store }, window, cx));
        let governance_participant_picker = cx.new(|cx| {
            gpui_component::combobox::ComboboxState::new(
                SearchableVec::<governance::ParticipantChoice>::new(Vec::new()),
                Vec::new(),
                window,
                cx,
            )
            .multiple(true)
            .searchable(true)
        });
        let governance_proposal_action_amount_input = new_text_input(window, cx, "amount");
        let governance_staking_delegate_input = new_text_input(window, cx, "delegate address");
        let address_book_search_input = new_text_input(window, cx, "search saved recipients");
        let favorite_broadcaster_input =
            new_text_input(window, cx, "favorite broadcaster 0zk address");
        let banned_broadcaster_input = new_text_input(window, cx, "banned broadcaster 0zk address");
        let address_book_label_input = new_text_input(window, cx, "recipient label");
        let address_book = AddressBookState::new(address_book_search_input, window, cx);
        let public_form = PublicAccountFormState::new(
            public_account_search_input,
            mimic_railway_shields_by_default,
            window,
            cx,
        );
        let repair_cache_block_input = new_text_input(window, cx, "0 = deployment block");
        let tx_search_input = new_text_input(window, cx, "search tx hash");
        Self {
            unlock_password_input,
            new_password_input,
            confirm_password_input,
            wallet_name_input,
            manage_wallet_label_input,
            add_wallet_password_input,
            hardware_wallet_restore_account_index_input,
            #[cfg(feature = "hardware")]
            hardware_profile_password_input,
            #[cfg(feature = "hardware")]
            hardware_profile_label_input,
            #[cfg(feature = "hardware")]
            hardware_profile_recovery_start_input,
            #[cfg(feature = "hardware")]
            hardware_profile_recovery_count_input,
            #[cfg(feature = "hardware")]
            hardware_profile_exact_index_input,
            #[cfg(feature = "hardware")]
            trezor_app_passphrase_input,
            #[cfg(feature = "hardware")]
            trezor_passphrase_mode_focus,
            import_mnemonic_input,
            wallet_activity,
            logs_search,
            governance_participant_picker,
            governance_proposal_action_amount_input,
            governance_staking_delegate_input,
            favorite_broadcaster_input,
            banned_broadcaster_input,
            address_book_label_input,
            address_book,
            public_form,
            repair_cache_block_input,
            tx_search_input,
        }
    }
}
