use super::*;

impl PrivateSwapsView {
    /// What a status adds while the job may wait on a hardware account's device. `None` for
    /// an account the wallet signs for itself.
    pub(super) fn public_device_hint(
        &self,
        command: &PublicSwapAuthorization,
        cx: &App,
    ) -> Option<String> {
        (command.source.source == PublicAccountSource::HardwareDerived).then(|| {
            format!(
                " · confirm on your {} when it asks",
                self.public_device_label(cx)
            )
        })
    }

    /// What the account does once its destination is ready, before anything is placed.
    pub(super) fn public_finishing_status(command: &PublicSwapAuthorization) -> SharedString {
        match &command.action {
            PublicSwapCommand::Swap { review, .. } if review.intent().order => {
                "Getting the order ready · this can take a minute".into()
            }
            _ => "Getting the deposit ready · this can take a minute".into(),
        }
    }

    /// What a hardware Public account's device is about to sign for an order, decoded: its
    /// signed approval when the order carries one, its proxy's bridge instructions, then the
    /// order. The device may show them as raw data.
    pub(super) fn review_public_signatures(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(execution) = self.public_execution.as_ref() else {
            return;
        };
        let (
            Some(signed),
            PublicSwapCommand::Swap {
                review, approval, ..
            },
        ) = (execution.signed.as_ref(), &execution.command.action)
        else {
            return;
        };
        let Some(batch) = signed.batch.as_ref() else {
            return;
        };
        let device = self.public_device_label(cx);
        let count = if signed.permit.is_some() {
            "three"
        } else {
            "two"
        };
        let groups = Arc::new(self.public_signature_groups(
            &execution.command.source,
            execution.command.operation,
            review,
            approval.sell_token,
            signed.permit.as_ref(),
            batch,
            signed.valid_to,
            cx,
        ));
        let view = cx.entity();
        window.open_wallet_dialog(cx, move |dialog, _, _| {
            let sign = view.clone();
            let close = view.clone();
            dialog
                .title(DEVICE_SIGNATURES_TITLE)
                .footer(dialog_footer(public_sign_label(device), true))
                .on_close(move |_, window, cx| {
                    close.update(cx, |view, cx| {
                        if view.public_execution.take().is_some() {
                            view.schedule_public_quote(window, cx);
                        }
                        cx.notify();
                    });
                })
                .on_ok(move |_, window, cx| {
                    sign.update(cx, |view, cx| view.submit_public_signed(true, window, cx));
                    true
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            app_muted_text(format!(
                                "Your {device} will ask for {count} signatures. It may show them as raw data, so check them here first."
                            ))
                            .whitespace_normal(),
                        )
                        .children(groups.iter().map(|(title, rows)| {
                            div()
                                .w_full()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .border_1()
                                .border_color(rgb(theme::BORDER_SUBTLE))
                                .bg(rgb(theme::SURFACE))
                                .child(app_muted_text(title.clone()).text_xs())
                                .children(rows.iter().map(|row| {
                                    detail_row(
                                        row.label.clone(),
                                        div()
                                            .flex()
                                            .items_baseline()
                                            .gap_1()
                                            .child(app_text(row.value.clone()))
                                            .children(row.address.clone().map(|address| {
                                                app_text(address)
                                                    .text_xs()
                                                    .font_family(theme::APP_MONO_FONT_FAMILY)
                                            })),
                                        false,
                                        None,
                                    )
                                }))
                        })),
                )
        });
    }

    /// The device a hardware Public account signs on, as its button and intro name it.
    fn public_device_label(&self, cx: &App) -> &'static str {
        self.root
            .upgrade()
            .and_then(|root| {
                root.read(cx)
                    .view_session
                    .as_ref()
                    .and_then(|view| view.hardware_profile_session())
                    .map(|session| hardware_device_label(session.device_kind))
            })
            .unwrap_or("device")
    }

    /// The groups a hardware review decodes for an order paid by `source`, numbered in the
    /// order the device signs them: the approval `permit` grants, for an order that carries
    /// one, the bridge instructions of `batch`, then the order of `review` that sells
    /// `sell_token`, valid until `valid_to`. `operation` is the destination stealth account.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::root::private_swap::form) fn public_signature_groups(
        &self,
        source: &PublicAccountMetadata,
        operation: ExecutorOperationId,
        review: &PublicSwapReview,
        sell_token: Address,
        permit: Option<&PublicSwapPermitTerms>,
        batch: &PublicSwapBatchTerms,
        valid_to: u32,
        cx: &App,
    ) -> Vec<(String, Vec<PublicSignatureRow>)> {
        let network = batch.destination_chain;
        let bought = self.token_symbol(batch.input_token, cx);
        let delivered = self.network_token_symbol(network, batch.output_token, cx);
        // What the destination gets for each whole token the proxy holds.
        let scale = self
            .token_decimals(batch.input_token, cx)
            .zip(self.chain_token_metadata(network, batch.output_token, cx))
            .and_then(|(decimals, output)| {
                let rate = unit_rate(batch.scale_denominator, decimals, batch.scale_numerator)?;
                Some(railgun_ui::format_token_amount(rate, output.decimals))
            });
        let account = self
            .public_records
            .iter()
            .find(|(_, record)| record.operation() == operation)
            .map_or_else(
                || "your stealth account".to_owned(),
                |(_, record)| format!("account #{}", record.index()),
            );
        // The form names the Sell token as the account's balance does.
        let sold = |token: Address, amount: U256| {
            self.form
                .as_ref()
                .filter(|form| form.sell == token)
                .map_or_else(
                    || self.token_amount(token, amount, cx),
                    |form| self.form_sell_amount(form, amount, cx),
                )
        };
        let approval = permit.map(|permit| {
            (
                "Approval for CoW",
                vec![
                    PublicSignatureRow::new("Token", self.token_symbol(permit.token, cx))
                        .naming(permit.token),
                    PublicSignatureRow::new("Spender", "CoW's vault relayer")
                        .naming(permit.spender),
                    PublicSignatureRow::new("Amount", sold(permit.token, permit.amount)),
                    PublicSignatureRow::new(
                        "Valid until",
                        local_date_time_label(u64::from(permit.deadline)),
                    ),
                ],
            )
        });
        approval
            .into_iter()
            .chain([
                (
                    "Bridge instructions for your CoW proxy",
                    vec![
                        PublicSignatureRow::new(
                            "Runs only if the proxy holds",
                            format!(
                                "at least {}",
                                self.token_amount(batch.guard_token, batch.guard_amount, cx)
                            ),
                        ),
                        PublicSignatureRow::new(
                            "Deposits into Across",
                            format!("all {bought} in the proxy"),
                        ),
                        PublicSignatureRow::new("Refunds go to", public_source_label(source))
                            .naming(batch.depositor),
                        PublicSignatureRow::new(
                            format!("Delivered on {}", network_name(network)),
                            scale.map_or_else(
                                || format!("{delivered} for the {bought} in the proxy"),
                                |scale| format!("at least {scale} {delivered} per {bought}"),
                            ),
                        ),
                        PublicSignatureRow::new(
                            "Recipient",
                            format!("Across handler, then {account}"),
                        ),
                        PublicSignatureRow::new(
                            "Valid until",
                            local_date_time_label(u64::from(batch.deadline)),
                        ),
                    ],
                ),
                (
                    "CoW order",
                    vec![
                        PublicSignatureRow::new("Sell", sold(sell_token, review.sell_amount())),
                        PublicSignatureRow::new(
                            "Buy at least",
                            // The signed order's amount, which a short bridge quote can raise.
                            self.token_amount(batch.guard_token, batch.guard_amount, cx),
                        ),
                        PublicSignatureRow::new("Receiver", "Your CoW proxy").naming(batch.proxy),
                        PublicSignatureRow::new(
                            "Valid until",
                            local_date_time_label(u64::from(valid_to)),
                        ),
                    ],
                ),
            ])
            .enumerate()
            .map(|(index, (title, rows))| (format!("{} · {title}", index + 1), rows))
            .collect()
    }

    pub(super) fn submit_public_signed(
        &mut self,
        hash_fallback_confirmed: bool,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(execution) = self.public_execution.take() else {
            return;
        };
        if !self.public_command_is_current(&execution.command)
            || !self
                .root
                .upgrade()
                .is_some_and(|root| execution.command.sessions_are_current(root.read(cx)))
        {
            return;
        }
        let device = self
            .public_device_hint(&execution.command, cx)
            .unwrap_or_default();
        self.public_status = Some(
            match &execution.command.action {
                PublicSwapCommand::Swap { review, .. } if review.intent().order => {
                    format!("Placing the order{device}")
                }
                _ => format!("Sending the deposit{device}"),
            }
            .into(),
        );
        self.start_public_job(
            execution.command.clone(),
            submit_public_execution(execution, hash_fallback_confirmed),
            window,
            cx,
        );
    }
}
