use super::*;

impl PrivateSwapsView {
    pub(super) fn form_primary(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.form.as_ref().is_some_and(|form| form.public.is_some()) {
            self.request_public_review(window, cx);
            return;
        }
        self.close_setup_settings(cx);
        let Some(form) = self.form.as_ref() else {
            return;
        };
        match self.form_mode(form) {
            FormMode::Setup { resume } => self.request_setup(resume, window, cx),
            // A reused account's swap that delivers to a new stealth account reviews that
            // account's setup, which is sent again once the pair is reserved.
            FormMode::Order if self.reviews_setup(form) => {
                let resume = self.destination_operation(form).is_some();
                self.request_setup(resume, window, cx);
            }
            FormMode::Order => {
                // A change still waiting for review is named when the user opens it.
                let change = self
                    .reapproval
                    .filter(|(pending, _)| form.operation == Some(*pending))
                    .map(|(_, change)| change);
                self.request_order_review(change, window, cx);
            }
            FormMode::SettingUp | FormMode::Placed => {}
        }
    }

    /// Resume a quote that waited for the setup's confirmation.
    pub(in crate::root::private_swap) fn continue_form_after_observation(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if self.form_mode(form) == FormMode::Order
            && (matches!(form.quote, QuoteState::Idle | QuoteState::Failed(_))
                || matches!(&form.quote, QuoteState::Ready(review)
                    if review.plan().swap_executor().requires_setup()))
            && form.quote_task.is_none()
        {
            self.schedule_quote(window, cx);
        }
    }

    pub(in crate::root::private_swap) fn focus_form_amount(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(form) = self.form.as_ref() {
            form.amount_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }
    }

    pub(in crate::root::private_swap) fn close_setup_settings(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        self.set_setup_settings_open(false, cx);
    }

    pub(super) fn set_setup_settings_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.settings_open != open
        {
            form.settings_open = open;
            cx.notify();
        }
    }

    pub(super) fn set_gas_help_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.gas_help_open != open
        {
            form.gas_help_open = open;
            cx.notify();
        }
    }

    pub(super) fn set_quote_details_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.quote_details_open != open
        {
            form.quote_details_open = open;
            cx.notify();
        }
    }

    pub(super) fn set_bridge_notice_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.bridge.notice_open != open
        {
            form.bridge.notice_open = open;
            cx.notify();
        }
    }

    pub(super) fn set_provider_hint_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.bridge.provider_hint_open != open
        {
            form.bridge.provider_hint_open = open;
            cx.notify();
        }
    }

    pub(super) fn set_failure_hint_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.bridge.failure_hint_open != open
        {
            form.bridge.failure_hint_open = open;
            cx.notify();
        }
    }

    pub(super) fn toggle_details(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.details_open = !form.details_open;
            cx.notify();
        }
    }
}
