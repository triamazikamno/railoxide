use super::*;

impl PrivateSwapsView {
    /// A token's symbol, decimals and icon. The native asset, which a Public address swap can
    /// buy as `Address::ZERO`, isn't in the token registry, so it comes from the chain.
    pub(super) fn token_metadata(
        &self,
        token: Address,
        cx: &gpui::App,
    ) -> Option<crate::root::tokens::TokenDisplayMetadata> {
        self.chain_token_metadata(self.origin_chain_id, token, cx)
    }

    /// [`Self::token_metadata`] on `chain_id`, such as a Bridge swap's destination network.
    pub(super) fn chain_token_metadata(
        &self,
        chain_id: u64,
        token: Address,
        cx: &gpui::App,
    ) -> Option<crate::root::tokens::TokenDisplayMetadata> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        if token == Address::ZERO {
            let native = &root.effective_chain_configs.get(chain_id)?.native_currency;
            return Some(crate::root::tokens::TokenDisplayMetadata {
                symbol: native.symbol.clone(),
                decimals: native.decimals,
                icon_path: railgun_ui::native_currency_icon_asset_path(chain_id)
                    .map(crate::assets::WalletIconSource::embedded),
            });
        }
        crate::root::token_display_metadata(Some(&root.effective_token_registry), chain_id, &token)
    }

    pub(super) fn token_symbol(&self, token: Address, cx: &gpui::App) -> String {
        self.token_metadata(token, cx)
            .map_or_else(|| railgun_ui::short_address(&token), |info| info.symbol)
    }

    pub(super) fn token_amount(&self, token: Address, amount: U256, cx: &gpui::App) -> String {
        if token == Address::ZERO
            && let Some(native) = self.token_metadata(token, cx)
        {
            return format!(
                "{} {}",
                railgun_ui::format_token_amount(amount, native.decimals),
                native.symbol
            );
        }
        self.root.upgrade().map_or_else(
            || amount.to_string(),
            |root| {
                crate::root::format_token_amount_for_display(
                    self.origin_chain_id,
                    token,
                    amount,
                    Some(&root.read(cx).effective_token_registry),
                )
            },
        )
    }

    pub(super) fn labels(&self, record: &ExecutorRecord, cx: &gpui::App) -> SwapLabels {
        if let Some(pending) = self.pending_order(record) {
            let labels = self.order_labels(
                record,
                (pending.sell, pending.buy),
                Some(pending.amount),
                None,
                None,
                cx,
            );
            let bridge = self.bridge_labels(record, &pending.delivery, pending.buy, None, None, cx);
            return SwapLabels {
                pair: bridge_pair_label(&labels, bridge.as_ref()),
                receiver: self.receiver_name(&pending.delivery, cx),
                minimum: Some(self.token_amount(pending.buy, pending.private_minimum, cx)),
                bridge,
                ..labels
            };
        }
        let Some((sell, buy)) = swap_tokens(record) else {
            return SwapLabels {
                pair: "Private swap".into(),
                sell: "Private swap".into(),
                buy_symbol: String::new(),
                expires: None,
                lapsed: false,
                fill_hint: None,
                received: None,
                receiver: None,
                minimum: None,
                uncovered_gas: None,
                order: None,
                shield_transaction: None,
                settlement_transaction: None,
                bridge: None,
            };
        };
        let amount = swap_sell_amount(record).or_else(|| {
            self.tracking
                .get(&record.operation())
                .and_then(|tracking| tracking.amount)
        });
        let order = record.swap().and_then(|swap| swap.orders().last());
        let labels = self.order_labels(
            record,
            (sell, buy),
            amount,
            order,
            self.fill_hint(record, cx),
            cx,
        );
        if order.is_some() {
            return labels;
        }
        // Before its first order, the swap delivers as approved with its setup.
        let delivery = swap_delivery(record);
        let approved = record.swap_approval().map(|approval| &approval.bounds);
        let bridge = self.bridge_labels(record, &delivery, buy, None, approved, cx);
        SwapLabels {
            pair: bridge_pair_label(&labels, bridge.as_ref()),
            receiver: self.receiver_name(&delivery, cx),
            minimum: model::swap_private_minimum(record)
                .map(|minimum| self.token_amount(buy, minimum, cx)),
            bridge,
            ..labels
        }
    }

    pub(super) fn setup_confirmation_detail(
        &self,
        record: &ExecutorRecord,
        cx: &gpui::App,
    ) -> Option<String> {
        let root = self.root.upgrade()?;
        let depth = root
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)?
            .finality_depth;
        // Debug UI fixture: its included setup counts from a head of its own, which nothing
        // finalizes, so the count holds still.
        #[cfg(debug_assertions)]
        if let Some((block, head)) = ui_fixture::setup_included() {
            return model::SwapSetupConfirmation::Included(block).detail(
                crate::root::utxo::UtxoFinalityContext::new(Some(head), Some(0), Some(depth)),
            );
        }
        let tip = *self.private_session()?.sync_tip_rx.borrow();
        self.setup_confirmation(record)?
            .detail(crate::root::utxo::UtxoFinalityContext::new(
                tip.head_block,
                tip.safe_head_block,
                Some(depth),
            ))
    }

    /// Display strings for one swap of `record`: `amount` of `sell` for `buy`, with `order`
    /// its latest order when it has one. The minimum, receiver, gas and ID are those of the
    /// attempt the swap follows, which among competing orders is the one that traded. The
    /// expiry is that attempt's, or the last of the orders that can still trade.
    pub(super) fn order_labels(
        &self,
        record: &ExecutorRecord,
        (sell, buy): (Address, Address),
        amount: Option<U256>,
        order: Option<&SwapOrderRecord>,
        fill_hint: Option<SwapFillHint>,
        cx: &gpui::App,
    ) -> SwapLabels {
        let sell_symbol = self.token_symbol(sell, cx);
        let buy_symbol = self.token_symbol(buy, cx);
        let sell_label = amount.map_or(sell_symbol, |amount| self.token_amount(sell, amount, cx));
        // What was delivered, and a Bridge swap's hand-off and destination terms, come from
        // the attempt that delivered, which a retry's trade can leave to an earlier attempt.
        let attempts = order.map(|order| model::SwapAttempts::ending_at(record, order));
        let evidence = attempts.map(|attempts| attempts.bridge_evidence());
        let order = attempts.map(|attempts| attempts.outcome);
        let received = evidence.and_then(|order| {
            let observed = order.observations();
            let amount = match order.delivery() {
                SwapDelivery::Reshield => observed.shielded.map(|shield| shield.private_amount),
                // The trade paid the receiver directly.
                SwapDelivery::External { .. } => observed
                    .delivered
                    .and(observed.trade_amounts)
                    .map(|trade| trade.buy_amount),
                // The destination network's token and amount, verified or reported.
                SwapDelivery::Bridge(bridge) => {
                    let amount = match observed.bridge_outcome? {
                        // The destination stealth account shielded this, less the shield fee.
                        SwapBridgeOutcome::DeliveredVerified {
                            output_amount,
                            shielded: true,
                            ..
                        } => Some(model::private_delivery_credit(
                            output_amount,
                            order.bounds(),
                        )),
                        SwapBridgeOutcome::DeliveredVerified { output_amount, .. } => {
                            Some(output_amount)
                        }
                        SwapBridgeOutcome::DeliveredReported { amount_out, .. } => amount_out,
                        // Held proceeds weren't received; the bridge labels name them.
                        SwapBridgeOutcome::Refunding
                        | SwapBridgeOutcome::NeedsAttention
                        | SwapBridgeOutcome::HeldOnDestination { .. } => None,
                    }?;
                    return Some(self.network_token_amount(
                        bridge.destination_chain,
                        self.delivered_token(bridge, cx),
                        amount,
                        cx,
                    ));
                }
                // 1Click's reported amount, in the record's token.
                SwapDelivery::NonEvmBridge(delivery) => {
                    let Some(SwapBridgeOutcome::DeliveredReported {
                        amount_out: Some(amount),
                        ..
                    }) = observed.bridge_outcome
                    else {
                        return None;
                    };
                    return Some(non_evm_amount(&delivery, amount));
                }
            };
            amount.map(|amount| self.token_amount(buy, amount, cx))
        });
        let valid_to = attempts.map(|attempts| attempts.valid_to());
        let mut bridge = evidence.and_then(|order| {
            self.bridge_labels(record, &order.delivery(), buy, Some(order), None, cx)
        });
        // The deposit read early and a provider's early report show only beside the reported
        // fill they follow.
        if fill_hint.is_some()
            && let Some(bridge) = &mut bridge
        {
            bridge.report = self.bridge_report(record, cx);
            bridge.deposit = SwapEarlyHint::seen(
                self.reported_order(record).and_then(|(_, _, early)| early),
                |settlement| {
                    settlement
                        .handoff
                        .map(|handoff| self.token_amount(buy, handoff.sent, cx))
                },
            );
        }
        SwapLabels {
            pair: swap_pair_label(
                &sell_label,
                &bridge
                    .as_ref()
                    .map_or_else(|| buy_symbol.clone(), SwapBridgeLabels::named_token),
            ),
            sell: sell_label,
            buy_symbol,
            expires: valid_to.map(local_time_label),
            lapsed: valid_to.is_some_and(|valid_to| valid_to < now_unix()),
            fill_hint,
            received,
            receiver: order.and_then(|order| self.receiver_name(&order.delivery(), cx)),
            minimum: order.map(|order| self.token_amount(buy, order.bounds().private_minimum, cx)),
            uncovered_gas: order
                .and_then(|order| {
                    let bounds = order.bounds();
                    bounds.gas_estimate?.checked_sub(bounds.gas_allowance?)
                })
                .filter(|rest| !rest.is_zero())
                .map(|rest| self.money(buy, rest, cx)),
            order: order.map(SwapOrderRecord::uid),
            // The delivery observation of a swap back to the private balance is its shield.
            shield_transaction: evidence
                .filter(|order| matches!(order.delivery(), SwapDelivery::Reshield))
                .and_then(|order| order.observations().delivered)
                .and_then(|delivered| delivered.transaction_hash),
            settlement_transaction: attempts
                .and_then(|attempts| attempts.traded)
                .and_then(|traded| traded.observations().traded?.transaction_hash),
            bridge,
        }
    }

    /// `amount` of `token` in the wallet's currency, "$4.42", or as a token amount without a
    /// cached rate.
    pub(super) fn money(&self, token: Address, amount: U256, cx: &gpui::App) -> String {
        self.usd_micro_value(token, amount, cx).map_or_else(
            || self.token_amount(token, amount, cx),
            railgun_ui::format_usd_micro_value,
        )
    }

    /// How swaps name a Bridge delivery of `buy`: its network, provider and receiver, and from
    /// `order`, what it handed to the bridge and its approved destination minimum. Before an
    /// order exists, `approved` holds the bounds approved with the setup. A private delivery
    /// also names both stealth accounts of `record`'s swap and what the destination account
    /// holds. `None` for delivery on this network.
    fn bridge_labels(
        &self,
        record: &ExecutorRecord,
        delivery: &SwapDelivery,
        buy: Address,
        order: Option<&SwapOrderRecord>,
        approved: Option<&SwapApprovedBounds>,
        cx: &gpui::App,
    ) -> Option<SwapBridgeLabels> {
        let bridge = match delivery {
            SwapDelivery::Bridge(bridge) => *bridge,
            SwapDelivery::NonEvmBridge(delivery) => {
                return Some(self.non_evm_bridge_labels(delivery, buy, order, approved, cx));
            }
            SwapDelivery::Reshield | SwapDelivery::External { .. } => return None,
        };
        let token = self.delivered_token(bridge, cx);
        let destination_amount =
            |amount| self.network_token_amount(bridge.destination_chain, token, amount, cx);
        let swap = self.order_swap(record, order);
        let private = bridge.is_private().then(|| SwapPrivateBridgeLabels {
            setups: self.setup_labels(record, swap, bridge, order.is_some(), cx),
            held: order.and_then(|order| match order.observations().bridge_outcome {
                Some(SwapBridgeOutcome::HeldOnDestination { amount, .. }) => {
                    Some(destination_amount(amount))
                }
                _ => None,
            }),
        });
        Some(SwapBridgeLabels {
            provider: bridge.provider,
            network: form::network_name(bridge.destination_chain),
            token: self.network_token_symbol(bridge.destination_chain, token, cx),
            non_evm: false,
            origin: form::network_name(self.origin_chain_id),
            receiver: self
                .receiver_label(bridge.receiver, cx)
                .map_or_else(|| short_receiver(bridge.receiver), |(label, _)| label),
            sent: order
                .and_then(bridge_sent_amount)
                .map(|amount| self.token_amount(buy, amount, cx)),
            minimum: order
                .map(SwapOrderRecord::bounds)
                .or(approved)
                .and_then(|bounds| Some((bounds, bounds.destination_minimum?)))
                .map(|(bounds, minimum)| {
                    // The private balance gets the minimum less the destination shield fee.
                    destination_amount(if bridge.is_private() {
                        model::private_delivery_credit(minimum, bounds)
                    } else {
                        minimum
                    })
                }),
            private,
            reported: order.is_some_and(|order| {
                matches!(
                    order.observations().bridge_outcome,
                    Some(SwapBridgeOutcome::DeliveredReported { .. })
                )
            }),
            delivered_transaction: order.and_then(|order| {
                match order.observations().bridge_outcome {
                    Some(
                        SwapBridgeOutcome::DeliveredVerified {
                            transaction_hash, ..
                        }
                        | SwapBridgeOutcome::HeldOnDestination {
                            transaction_hash, ..
                        },
                    ) => Some(transaction_hash),
                    Some(SwapBridgeOutcome::DeliveredReported {
                        transaction_hash, ..
                    }) => transaction_hash,
                    _ => None,
                }
            }),
            destination_transaction: None,
            handoff_transaction: order.and_then(|order| {
                order
                    .observations()
                    .bridge_handoff?
                    .observation
                    .transaction_hash
            }),
            refund_transaction: order
                .and_then(|order| order.observations().bridge_refund?.transaction_hash),
            deposit: SwapSeen::Looking,
            report: None,
        })
    }

    /// [`Self::bridge_labels`] of a non-EVM delivery, from its record alone, so it needs no
    /// token list: the network's name, the receiver's saved label on that network or its short
    /// form, the token's symbol, and amounts in its decimals. 1Click's destination transaction
    /// is kept as it reports it.
    fn non_evm_bridge_labels(
        &self,
        delivery: &NonEvmDelivery,
        buy: Address,
        order: Option<&SwapOrderRecord>,
        approved: Option<&SwapApprovedBounds>,
        cx: &gpui::App,
    ) -> SwapBridgeLabels {
        let observed = order.map(SwapOrderRecord::observations);
        SwapBridgeLabels {
            provider: BridgeProvider::NearIntents,
            network: delivery.network.display_name().to_owned(),
            token: delivery.symbol.clone(),
            non_evm: true,
            origin: form::network_name(self.origin_chain_id),
            receiver: self
                .non_evm_receiver_label(delivery.network, &delivery.receiver, cx)
                .unwrap_or_else(|| railgun_ui::short_non_evm(&delivery.receiver)),
            sent: order
                .and_then(bridge_sent_amount)
                .map(|amount| self.token_amount(buy, amount, cx)),
            minimum: order
                .map(SwapOrderRecord::bounds)
                .or(approved)
                .and_then(|bounds| bounds.destination_minimum)
                .map(|minimum| non_evm_amount(delivery, minimum)),
            private: None,
            reported: observed.as_ref().is_some_and(|observed| {
                matches!(
                    observed.bridge_outcome,
                    Some(SwapBridgeOutcome::DeliveredReported { .. })
                )
            }),
            // The destination transaction isn't an EVM hash.
            delivered_transaction: None,
            destination_transaction: observed.as_ref().and_then(|observed| {
                match &observed.bridge_outcome {
                    Some(SwapBridgeOutcome::DeliveredReported {
                        destination_transaction,
                        ..
                    }) => destination_transaction.clone(),
                    _ => None,
                }
            }),
            handoff_transaction: observed
                .as_ref()
                .and_then(|observed| observed.bridge_handoff?.observation.transaction_hash),
            refund_transaction: observed
                .as_ref()
                .and_then(|observed| observed.bridge_refund?.transaction_hash),
            deposit: SwapSeen::Looking,
            report: None,
        }
    }

    /// What its bridge provider reported early about the order of `record` whose fill the
    /// orderbook reports, with a delivered amount in the destination network's token, and for
    /// a private delivery what its fill block showed: the private credit after the shield fee,
    /// or what the destination stealth account holds.
    fn bridge_report(&self, record: &ExecutorRecord, cx: &gpui::App) -> Option<SwapBridgeReported> {
        use wallet_ops::{SwapBridgeReport, SwapDeliverySeen};
        let hints = &self.tracking.get(&record.operation())?.early_hints;
        let (order, delivery, report) =
            model::tracked_orders(record)
                .into_iter()
                .find_map(|order| {
                    let delivery = order.delivery();
                    if !delivery.is_bridge() {
                        return None;
                    }
                    Some((order, delivery, hints.get(&order.uid())?.report?))
                })?;
        // A non-EVM delivery's amounts are in its record's token and decimals.
        let shown = |amount: U256| match &delivery {
            SwapDelivery::Bridge(delivery) => self.network_token_amount(
                delivery.destination_chain,
                self.delivered_token(*delivery, cx),
                amount,
                cx,
            ),
            SwapDelivery::NonEvmBridge(delivery) => non_evm_amount(delivery, amount),
            SwapDelivery::Reshield | SwapDelivery::External { .. } => amount.to_string(),
        };
        Some(match report {
            SwapBridgeReport::Delivered {
                amount_out,
                seen,
                transaction,
            } => SwapBridgeReported::Delivered {
                amount: amount_out.map(shown),
                shield: match seen {
                    None => SwapShieldSeen::Looking,
                    Some(SwapDeliverySeen::Shielded { amount }) => SwapShieldSeen::Detected(shown(
                        model::private_delivery_credit(amount, order.bounds()),
                    )),
                    Some(SwapDeliverySeen::Held { amount }) => SwapShieldSeen::Held(shown(amount)),
                },
                transaction,
            },
            SwapBridgeReport::Pending => SwapBridgeReported::Pending,
            SwapBridgeReport::Refunded => SwapBridgeReported::Refunded,
            SwapBridgeReport::Failed => SwapBridgeReported::Failed,
        })
    }

    /// The token a Bridge `delivery` pays out, as swaps name it. A private delivery shields
    /// the destination token itself, so a wrapped native token stays wrapped.
    pub(super) fn delivered_token(&self, delivery: BridgeDelivery, cx: &gpui::App) -> Address {
        if delivery.is_private() {
            delivery.destination_token
        } else {
            self.bridge_received_token(delivery, cx)
        }
    }

    /// The setup of each stealth account of `record`'s private Bridge swap `swap`, the swap's
    /// own network first. `placed` tells that the order these labels describe exists, which
    /// needs both accounts set up. The swap's own setup is finished once this session's latest
    /// read shows it included, as the detail shows it.
    fn setup_labels(
        &self,
        record: &ExecutorRecord,
        swap: Option<SwapIdentity>,
        delivery: BridgeDelivery,
        placed: bool,
        cx: &gpui::App,
    ) -> [SwapSetupLabels; 2] {
        let operation = record.operation();
        let tracking = self.tracking.get(&operation);
        let origin = if placed || self.setup_inclusion(record).is_some() {
            SwapSetupProgress::Done
        } else {
            account_setup_progress(
                record,
                self.origin_chain_id,
                tracking.and_then(|tracking| tracking.setup),
                self.submitting_setup(operation),
            )
        };
        // What this session knows the swap's own setup waits for.
        let waiting = match origin {
            SwapSetupProgress::Submitting => tracking
                .and_then(|tracking| tracking.setup_progress(SetupSide::Origin)?.stage())
                .map(|stage| stage.label().to_owned()),
            SwapSetupProgress::Pending => self.setup_confirmation_detail(record, cx),
            _ => None,
        };
        let destination = swap.and_then(|swap| self.swap_destination_account(swap, delivery));
        // An account its swap use didn't reserve fresh is reused: the use sends no setup for
        // it. A draft tells before its use claims the account, and the saved approval tells
        // of a destination account whose network isn't loaded.
        let swap_use = swap.and_then(|swap| record.swap_use(swap.swap_use));
        let reuses = |account: &ExecutorRecord| {
            swap.and_then(|swap| account.swap_use(swap.swap_use))
                .is_some_and(|swap_use| !swap_use.is_fresh())
        };
        let source_reused = reuses(record)
            || self.pending_order(record).is_some_and(|pending| {
                pending.reuse_account && swap.is_some_and(|swap| swap.swap_use == pending.swap_use)
            });
        let destination = destination_account_metadata(
            destination,
            swap.map(|swap| swap.swap_use),
            swap_use.and_then(SwapUseRecord::approval),
        );
        let destination_fresh = destination.fresh_from_record_or_approval();
        [
            SwapSetupLabels {
                network: form::network_name(self.origin_chain_id),
                account: record.address().map(|address| SwapStepAccount {
                    index: Some(record.index()),
                    address,
                }),
                progress: origin,
                reused: source_reused,
                detail: waiting,
            },
            SwapSetupLabels {
                network: form::network_name(delivery.destination_chain),
                account: (!delivery.receiver.is_zero()).then(|| SwapStepAccount {
                    index: destination.record.map(ExecutorRecord::index),
                    address: delivery.receiver,
                }),
                progress: if placed {
                    SwapSetupProgress::Done
                } else {
                    self.destination_setup_progress(record, delivery)
                },
                reused: !destination_fresh,
                detail: None,
            },
        ]
    }

    /// How swaps name a Public address receiver: the wallet's label for it, or its short
    /// address. `None` for a swap back to the private balance.
    pub(super) fn receiver_name(&self, delivery: &SwapDelivery, cx: &gpui::App) -> Option<String> {
        let &SwapDelivery::External { receiver } = delivery else {
            return None;
        };
        Some(
            self.receiver_label(receiver, cx)
                .map_or_else(|| short_receiver(receiver), |(label, _)| label),
        )
    }
}
