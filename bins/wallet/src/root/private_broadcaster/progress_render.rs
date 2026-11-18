use super::*;

impl WalletRoot {
    pub(in crate::root) fn render_private_broadcaster_progress_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
        cx: &gpui::App,
    ) -> gpui::Div {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return div()
                .w(content_width)
                .child(app_muted_text("No active private submission."));
        };
        let refusal_actions = self
            .refused_public_broadcaster_retry(progress.kind, progress.key, progress.generation_id)
            .map(|(_, choice)| *choice == BroadcasterChoice::Random);
        let input_pin_override =
            self.input_pin_conflict_retryable(progress.kind, progress.key, progress.generation_id);
        let mut content = div().w(content_width).flex().flex_col().gap_3().child(
            render_private_broadcaster_progress_stepper(
                root,
                progress,
                refusal_actions,
                input_pin_override,
            ),
        );

        match progress.flow {
            PrivateSubmissionProgressFlow::PublicBroadcaster => {
                if let Some(context) = self.private_broadcaster_progress_context(progress) {
                    let broadcaster_action =
                        self.render_public_broadcaster_preference_action(root, progress);
                    content = content.child(render_private_broadcaster_progress_context(
                        progress,
                        &context,
                        broadcaster_action,
                    ));
                } else {
                    content =
                        content.child(render_pending_public_broadcaster_progress_context(progress));
                }
            }
            PrivateSubmissionProgressFlow::SelfBroadcast => {
                content = content.child(render_self_broadcast_progress_context(progress));
            }
        }

        if let Some(result) = progress.result.as_ref()
            && let PublicBroadcasterResultKind::Submitted { tx_hash } = &result.result
        {
            content = content.child(render_public_broadcaster_tx_hash_row(
                tx_hash.clone(),
                delivery_element_id(progress.key, progress.kind, "progress-copy-public-tx"),
            ));
        }
        if let Some(result) = progress.self_broadcast_result.as_ref() {
            content = content.child(render_public_broadcaster_tx_hash_row(
                result.tx.tx_hash().to_owned(),
                delivery_element_id(progress.key, progress.kind, "progress-copy-self-tx"),
            ));
        }
        if private_broadcaster_progress_is_terminal(progress)
            && let Some(target) = &progress.stealth_account
        {
            let target = target.clone();
            let recovery_root = root.clone();
            content = content.child(
                app_button(
                    delivery_element_id(progress.key, progress.kind, "progress-stealth-account"),
                    "View stealth account",
                )
                .outline()
                .disabled(!self.can_open_stealth_account(&target, cx))
                .on_click(move |_, window, cx| {
                    recovery_root.update(cx, |root, cx| {
                        if root.can_open_stealth_account(&target, cx) {
                            root.close_private_broadcaster_progress_dialog(window, cx);
                            window.close_all_dialogs(cx);
                            root.open_stealth_account(&target, window, cx);
                        }
                    });
                }),
            );
        }
        #[cfg(feature = "hardware")]
        if let Some(prompt) = self
            .hardware_profile_unlock
            .trezor_pin_matrix_prompt
            .as_ref()
        {
            content = content.child(crate::root::vault_ui::render_trezor_pin_matrix_prompt(
                root, prompt,
            ));
        }
        content = content.child(render_private_broadcaster_progress_footer(
            root.clone(),
            progress,
        ));
        content
    }

    pub(super) fn private_broadcaster_progress_context<'a>(
        &'a self,
        progress: &'a PrivateBroadcasterProgressState,
    ) -> Option<PrivateBroadcasterProgressContext<'a>> {
        if let Some(result) = progress.result.as_ref() {
            let anchor_rate = self
                .public_broadcaster_anchor_cache
                .cached_rate(result.broadcaster.chain_id, result.fee_token);
            return Some(PrivateBroadcasterProgressContext {
                display: PublicBroadcasterCostDisplay::from_result(
                    result,
                    anchor_rate,
                    Some(&self.effective_token_registry),
                ),
                anchor_cache: &self.public_broadcaster_anchor_cache,
            });
        }
        let estimate = progress.estimate.as_ref()?;
        let anchor_rate = self
            .public_broadcaster_anchor_cache
            .cached_rate(progress.key.chain_id, estimate.fee_token);
        Some(PrivateBroadcasterProgressContext {
            display: PublicBroadcasterCostDisplay::from_estimate_chain(
                progress.key.chain_id,
                estimate,
                anchor_rate,
                Some(&self.effective_token_registry),
            ),
            anchor_cache: &self.public_broadcaster_anchor_cache,
        })
    }

    fn render_public_broadcaster_preference_action(
        &self,
        root: &Entity<Self>,
        progress: &PrivateBroadcasterProgressState,
    ) -> Option<gpui::AnyElement> {
        let address = public_broadcaster_progress_address(progress)?;
        let submitted = progress.result.as_ref().is_some_and(|result| {
            matches!(
                &result.result,
                PublicBroadcasterResultKind::Submitted { .. }
            )
        });
        let control = broadcaster_preference_control(
            self.is_banned_broadcaster(address),
            self.is_favorite_broadcaster(address),
            submitted,
            public_broadcaster_waiting_can_stop(progress) && !progress.stop_available,
        )?;
        Some(render_broadcaster_preference_control(
            control,
            delivery_element_id(progress.key, progress.kind, "broadcaster-preference"),
            address,
            root.downgrade(),
            |_| {},
        ))
    }
}
