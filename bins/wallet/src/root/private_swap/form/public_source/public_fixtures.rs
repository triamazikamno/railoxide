#[allow(clippy::wildcard_imports)]
use super::*;

impl PrivateSwapsView {
    /// Hold the route lookup failure on a scratch wallet for a real-window screenshot.
    #[cfg(debug_assertions)]
    pub(in crate::root::private_swap::form) fn stage_public_route_error_for_fixture(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !ui_fixture::route_error() {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(public) = form.public.as_mut() else {
            return;
        };
        form.network = Some(42_161);
        form.buy = Some(alloy::primitives::address!(
            "fd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9"
        ));
        form.quote_task = None;
        form.quote_revision = form.quote_revision.wrapping_add(1);
        form.quote = QuoteState::Idle;
        form.amount_input
            .update(cx, |input, cx| input.set_value("7", window, cx));
        public.route_tasks.clear();
        public.orders_task = None;
        public.orders_available = Some(true);
        public.routes.clear();
        public.route_errors.insert(
            (form.sell, 42_161),
            "Across rejected the routes request (HTTP 403).".into(),
        );
        self.refresh_form_delivery(cx);
        cx.notify();
    }

    /// Take the account's chain as one that takes orders or not, in place of the form's read.
    #[cfg(test)]
    pub(in crate::root::private_swap::form) fn stub_public_orders_for_tests(
        &mut self,
        available: bool,
        cx: &App,
    ) {
        let form = self.form.as_mut().expect("test form");
        let public = form.public.as_mut().expect("Public test form");
        public.orders_task = None;
        public.orders_available = Some(available);
        self.list_public_routes(cx);
    }

    #[cfg(test)]
    pub(in crate::root::private_swap::form) fn install_public_review_for_tests(
        &mut self,
        review: PublicSwapReview,
        across: AcrossClient,
        orderbook: Option<CowOrderbookClient>,
        cx: &App,
    ) {
        // A reviewed swap is on a chain whose orders were read as available.
        self.stub_public_orders_for_tests(true, cx);
        let origin = self.public_origin(cx).expect("test origin");
        let form = self.form.as_ref().expect("test form");
        let amount = self.form_amount(form, cx).expect("test amount");
        let terms = PublicQuoteTerms {
            source: form
                .public
                .as_ref()
                .expect("Public test form")
                .account
                .clone(),
            origin,
            network: form.network.expect("test destination"),
            sell: form.sell,
            buy: form.buy.expect("test token"),
            amount,
            slippage_bps: form.slippage_bps,
            gas_share_bps: form.gas_share_bps,
            shield_failure: form.bridge.shield_failure,
        };
        let form = self.form.as_mut().expect("test form");
        form.quote_task = None;
        form.quote = QuoteState::Idle;
        let public = form.public.as_mut().expect("Public test form");
        public.review = Some(Arc::new(review));
        public.quote_terms = Some(terms);
        let http = orderbook.as_ref().expect("test orderbook").http().clone();
        public.clients = Some(PublicSwapClients {
            across,
            orderbook,
            http,
        });
    }

    #[cfg(test)]
    pub(in crate::root::private_swap::form) fn hold_public_preparation_for_tests(
        &mut self,
        command: Arc<PublicSwapAuthorization>,
        gate: tokio::sync::oneshot::Receiver<()>,
        completed: Arc<std::sync::atomic::AtomicUsize>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> eyre::Result<crate::root::private_swap::model::SwapIdentity> {
        let PublicSwapCommand::Swap {
            account,
            destination_token,
            review,
            approval,
            ..
        } = &command.action
        else {
            return Err(eyre::eyre!("Expected a preparation command."));
        };
        command.owner.claim_public_swap(PublicSwapUseClaim {
            id: command.swap_use,
            origin_chain: command.origin.chain_id,
            source: command.source.address,
            source_scope: command.source.scope.clone(),
            account: *account,
            destination_token: *destination_token,
            intent: review.intent(),
            approval: approval.clone(),
        })?;
        let public = self
            .form
            .as_mut()
            .and_then(|form| form.public.as_mut())
            .expect("Public test form");
        public.operation = Some(command.operation);
        public.swap_use = Some(command.swap_use);
        self.public_authorization = None;
        let identity = crate::root::private_swap::model::SwapIdentity {
            operation: command.operation,
            swap_use: command.swap_use,
        };
        self.start_public_job(
            command,
            async move {
                gate.await?;
                completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(PublicExecutionResult::Finished(identity))
            },
            window,
            cx,
        );
        Ok(identity)
    }

    #[cfg(test)]
    pub(in crate::root::private_swap::form) fn return_public_review_change_for_tests(
        &mut self,
        command: &PublicSwapAuthorization,
        review: PublicSwapReview,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> eyre::Result<()> {
        let PublicSwapCommand::Swap {
            account,
            destination_token,
            review: original,
            approval,
            ..
        } = &command.action
        else {
            return Err(eyre::eyre!("Expected a swap command."));
        };
        command.owner.claim_public_swap(PublicSwapUseClaim {
            id: command.swap_use,
            origin_chain: command.origin.chain_id,
            source: command.source.address,
            source_scope: command.source.scope.clone(),
            account: *account,
            destination_token: *destination_token,
            intent: original.intent(),
            approval: approval.clone(),
        })?;
        self.public_authorization = None;
        self.refresh_public_swap_records(cx);
        self.apply_public_review_change(
            command,
            SwapReviewChange::DestinationMinimum {
                approved: original.bridge().received_minimum(),
                current: review.bridge().received_minimum(),
            },
            Some(Arc::new(review)),
            window,
            cx,
        );
        Ok(())
    }

    /// The setup the debug UI fixture stands in for a new destination account that has no
    /// estimate: a fee in the token delivered there. `None` unless the fixture is on.
    pub(super) fn ui_fixture_setup(&self, form: &SwapForm, cx: &App) -> Option<SetupFee> {
        if !ui_fixture::active() || form.destination_route.estimate.is_some() {
            return None;
        }
        let chain_id = self.destination_setup_chain(form)?;
        let token = form.buy?;
        let decimals = self.chain_token_metadata(chain_id, token, cx)?.decimals;
        Some(SetupFee {
            chain_id,
            token,
            maximum: ui_fixture::setup_fee(decimals)?,
            broadcaster: "a fixture broadcaster".to_owned(),
        })
    }

    /// Debug UI fixture: put the synthesized record of the swap `command` reviewed where its
    /// claim would be.
    #[cfg(debug_assertions)]
    pub(super) fn stage_ui_fixture_swap(&mut self, command: &PublicSwapAuthorization) {
        let PublicSwapCommand::Swap {
            destination_token,
            review,
            approval,
            ..
        } = &command.action
        else {
            return;
        };
        self.stage_ui_fixture_flow(
            command.identity(),
            command.destination_session.chain_id,
            &ui_fixture::Staged {
                origin: command.origin.chain_id,
                source: command.source.address,
                sell: approval.sell_token,
                bridged: review.intent().bridged_token,
                delivered: *destination_token,
                order: review.intent().order,
                sold: review.sell_amount(),
                bought: review.buy_amount().unwrap_or_else(|| review.sell_amount()),
                received: approval.bounds.private_minimum,
            },
        );
    }
}
