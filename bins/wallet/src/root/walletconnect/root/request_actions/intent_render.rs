use super::*;

pub(super) fn append_walletconnect_simulation_risk(
    request_key: &str,
    fee_state: Option<&WalletConnectFeeState>,
    risks: &mut Vec<WalletConnectRisk>,
) {
    if let Some(state) = fee_state.filter(|state| state.request_key.as_ref() == request_key)
        && let Some(risk) = walletconnect_simulation_risk(&state.status, state.error.as_deref())
    {
        risks.push(risk);
    }
}

pub(super) fn render_walletconnect_intent_risk(
    request_key: &str,
    risk_index: usize,
    risk: &WalletConnectRisk,
) -> impl IntoElement {
    let message = match risk {
        WalletConnectRisk::UnlimitedAllowance { spender } => format!(
            "Unlimited allowance: {} can keep spending this token until the allowance is revoked.",
            spender.to_checksum(None)
        ),
        WalletConnectRisk::ForeignTransferSource { source } => format!(
            "This transferFrom call attempts to move funds from {}.",
            source.to_checksum(None)
        ),
        WalletConnectRisk::AttachedNativeValue(amount) => format!(
            "This token operation also sends {} to the contract.",
            amount.display
        ),
        WalletConnectRisk::UndecodedContractCall { selector } => format!(
            "This contract call could not be decoded{}. Inspect the raw request before approving.",
            selector.map_or_else(String::new, |selector| {
                format!(" (selector 0x{})", alloy::hex::encode(selector))
            })
        ),
        WalletConnectRisk::WouldRevert { reason } => format!("Would revert: {reason}"),
    };
    let id = SharedString::from(format!(
        "walletconnect-request-risk-{request_key}-{risk_index}"
    ));
    match risk {
        WalletConnectRisk::UnlimitedAllowance { .. } | WalletConnectRisk::WouldRevert { .. } => {
            Alert::error(id, message).small()
        }
        WalletConnectRisk::ForeignTransferSource { .. }
        | WalletConnectRisk::AttachedNativeValue(..)
        | WalletConnectRisk::UndecodedContractCall { .. } => Alert::warning(id, message).small(),
    }
}

pub(super) fn render_walletconnect_intent_card(
    request_key: &str,
    intent: &WalletConnectIntentView<'_>,
    chains: &EffectiveChainRegistry,
    content_width: Pixels,
) -> gpui::Div {
    let mut card = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_ELEVATED))
        .p(px(12.0))
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    app_strong_text(intent.hero.verb)
                        .text_size(px(18.0))
                        .whitespace_normal(),
                )
                .when(
                    intent.action != WalletConnectIntentAction::ChainSwitch,
                    |this| {
                        this.child(walletconnect_approved_chain_chip(
                            &walletconnect_intent_chain_display(intent.chain_id, chains),
                        ))
                    },
                ),
        )
        .when_some(
            render_walletconnect_intent_hero(intent, chains),
            gpui::ParentElement::child,
        );

    if walletconnect_should_render_token_contract(intent.action)
        && let Some(token) = walletconnect_intent_token_contract(&intent.amount)
    {
        card = card.child(render_walletconnect_address_row(
            "Token contract",
            None,
            token,
            Some(walletconnect_token_contract_recognition(&intent.amount)),
            request_key,
            content_width,
        ));
    }

    let party_count = intent.parties.len();
    let mut party_flow = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_3()
        .pt(px(6.0));
    for (index, party) in intent.parties.iter().enumerate() {
        party_flow = party_flow.child(render_walletconnect_party_row(
            party,
            request_key,
            content_width,
        ));
        if intent.action.allows_party_connector() && index + 1 < party_count {
            party_flow = party_flow.child(
                div()
                    .w_full()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(gpui_component::Icon::new(IconName::ArrowDown)),
            );
        }
    }
    if party_count > 0 {
        card = card.child(party_flow);
    }
    if let Some(attached_native) = intent.attached_native.as_ref()
        && !matches!(
            &intent.hero.summary,
            WalletConnectHeroSummary::UndecodedCall { .. }
        )
    {
        card = card.child(
            app_muted_text(format!(
                "Attached native value: {}",
                attached_native.display
            ))
            .whitespace_normal(),
        );
    }
    if let Some(effect) = intent.hero.approval_effect.as_deref() {
        card = card.child(
            Alert::new(
                SharedString::from(format!("walletconnect-approval-effect-{request_key}")),
                effect.to_owned(),
            )
            .small()
            .text_size(APP_TEXT_SIZE)
            .text_color(rgb(theme::TEXT_MUTED)),
        );
    }
    card
}

fn walletconnect_intent_chain_display(
    chain_id: u64,
    chains: &EffectiveChainRegistry,
) -> WalletConnectApprovedChainDisplay {
    let mut display = approved_chain_display_item(&format!("eip155:{chain_id}"));
    if let Some(chain) = chains.get(chain_id) {
        display.label.clone_from(&chain.name);
    }
    display
}

fn render_walletconnect_intent_hero(
    intent: &WalletConnectIntentView<'_>,
    chains: &EffectiveChainRegistry,
) -> Option<gpui::Div> {
    let mut hero = div().w_full().min_w(px(0.0)).flex().items_center().gap_3();
    if let Some(path) = intent.icon.clone() {
        hero = hero.child(img(path).size(px(34.0)).rounded_full().flex_none());
    }
    let mut body = div().min_w(px(0.0)).flex_1().flex().flex_col().gap_1();
    match &intent.hero.summary {
        WalletConnectHeroSummary::Amount => {
            let (label, danger) = walletconnect_intent_amount_label(&intent.amount);
            body = body.child(
                app_strong_text(label)
                    .text_size(px(22.0))
                    .text_color(rgb(if danger { theme::DANGER } else { theme::TEXT }))
                    .whitespace_normal(),
            );
            if let Some(context) = intent.usd_context.as_deref() {
                body = body.child(app_muted_text(walletconnect_approximate_usd_label(context)));
            }
        }
        WalletConnectHeroSummary::PersonalMessage(summary) => {
            body = body.child(app_strong_text(format!("{} bytes", summary.bytes)));
            body = body.child(app_muted_text(summary.preview.as_deref().map_or_else(
                || "Binary or control-heavy message; inspect raw request".to_owned(),
                |preview| format!("Message: {preview}"),
            )));
        }
        WalletConnectHeroSummary::TypedData(summary) => {
            if matches!(intent.action, WalletConnectIntentAction::Approve) {
                let (label, danger) = walletconnect_intent_amount_label(&intent.amount);
                body = body.child(
                    app_strong_text(label)
                        .text_size(px(22.0))
                        .text_color(rgb(if danger { theme::DANGER } else { theme::TEXT }))
                        .whitespace_normal(),
                );
                if let Some(context) = intent.usd_context.as_deref() {
                    body = body.child(app_muted_text(walletconnect_approximate_usd_label(context)));
                }
            }
            body = body.child(app_strong_text(
                summary
                    .domain_name
                    .as_deref()
                    .unwrap_or("No domain name supplied")
                    .to_owned(),
            ));
            body = body.child(app_muted_text(format!(
                "Primary type: {}",
                summary.primary_type
            )));
        }
        WalletConnectHeroSummary::UndecodedCall { .. } => {
            let attached_native = intent.attached_native.as_ref()?;
            body = body.child(
                app_strong_text(attached_native.display.clone())
                    .text_size(px(22.0))
                    .whitespace_normal(),
            );
            if let Some(usd) = attached_native.usd.as_deref() {
                body = body.child(app_muted_text(walletconnect_approximate_usd_label(usd)));
            }
        }
        WalletConnectHeroSummary::Policy(detail) => {
            body = body.child(app_muted_text(detail.clone()));
        }
        WalletConnectHeroSummary::ChainSwitch {
            from_chain_id,
            to_chain_id,
        } => {
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(walletconnect_approved_chain_chip(
                        &walletconnect_intent_chain_display(*from_chain_id, chains),
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(app_muted_text("→"))
                            .child(walletconnect_approved_chain_chip(
                                &walletconnect_intent_chain_display(*to_chain_id, chains),
                            )),
                    ),
            );
        }
        WalletConnectHeroSummary::None => {
            body = body.child(app_strong_text("Review request details"));
        }
    }
    Some(hero.child(body))
}

fn walletconnect_intent_amount_label(amount: &WalletConnectAmount) -> (String, bool) {
    match amount {
        WalletConnectAmount::KnownToken { display, .. }
        | WalletConnectAmount::Native { display, .. }
        | WalletConnectAmount::Unlimited { display, .. }
        | WalletConnectAmount::RawToken { display, .. } => (
            display.clone(),
            matches!(amount, &WalletConnectAmount::Unlimited { .. }),
        ),
        WalletConnectAmount::None => ("No amount specified".to_owned(), false),
    }
}

const fn walletconnect_intent_token_contract(amount: &WalletConnectAmount) -> Option<Address> {
    match amount {
        WalletConnectAmount::KnownToken { token, .. }
        | WalletConnectAmount::Unlimited { token, .. }
        | WalletConnectAmount::RawToken { token, .. } => Some(*token),
        WalletConnectAmount::Native { .. } | WalletConnectAmount::None => None,
    }
}

fn render_walletconnect_party_row(
    party: &WalletConnectParty,
    request_key: &str,
    content_width: Pixels,
) -> gpui::Div {
    let badge = walletconnect_party_badge_label(party.role, &party.badge);
    render_walletconnect_address_row(
        walletconnect_party_role_label(party.role),
        Some(party.role),
        party.address,
        badge.as_deref(),
        request_key,
        content_width,
    )
}

pub(super) fn render_walletconnect_address_row(
    role: &'static str,
    party_role: Option<WalletConnectPartyRole>,
    address: Address,
    badge: Option<&str>,
    request_key: &str,
    content_width: Pixels,
) -> gpui::Div {
    let full_address = address.to_checksum(None);
    let copy_id = SharedString::from(format!("walletconnect-request-{request_key}-{role}-copy"));
    let body = div()
        .min_w(px(0.0))
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            app_muted_text(role)
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::TEXT_SUBTLE)),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    app_strong_text(walletconnect_party_address_label(
                        party_role.unwrap_or(WalletConnectPartyRole::Contract),
                        &address,
                        content_width,
                    ))
                    .font_family(APP_MONO_FONT_FAMILY),
                )
                .when_some(badge, |this, badge| {
                    this.child(walletconnect_party_badge_chip(badge))
                })
                .child(clipboard_with_toast(copy_id, full_address)),
        );
    div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .items_start()
        .gap_2()
        .child(body)
}

fn walletconnect_party_badge_chip(label: &str) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
        .px(px(5.0))
        .py(px(2.0))
        .text_size(px(11.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .whitespace_normal()
        .child(SharedString::from(label.to_owned()))
}

pub(super) fn render_walletconnect_request_provenance(
    request: &WalletConnectRequestUi,
    intent: &WalletConnectIntentView<'_>,
    public_accounts: &[PublicAccountMetadata],
) -> gpui::Div {
    let (expiry, expiry_color) = match walletconnect_request_expiry_status(
        request.item.expiry_timestamp,
        current_unix_seconds(),
    ) {
        WalletConnectRequestExpiryStatus::Missing => {
            ("No request expiry supplied".to_owned(), theme::TEXT)
        }
        WalletConnectRequestExpiryStatus::Remaining(seconds) => (
            format!("Expires in {}", walletconnect_duration_label(seconds)),
            if seconds < 30 {
                theme::WARNING
            } else {
                theme::TEXT
            },
        ),
        WalletConnectRequestExpiryStatus::Expired => {
            ("Expired at deadline".to_owned(), theme::TEXT)
        }
    };
    div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .px(px(2.0))
        .child(walletconnect_kv_row(
            "Site",
            if request.request_control.is_some() {
                request.binding.peer_url.clone()
            } else {
                intent.provenance.site.clone()
            },
        ))
        .when(request.request_control.is_none(), |this| {
            this.when_some(intent.provenance.dapp_name.as_ref(), |this, name| {
                this.child(walletconnect_provenance_dapp_row(name))
            })
        })
        .when_some(
            request.item.account.filter(|account| {
                walletconnect_selected_account_provenance_visible(*account, &intent.parties)
            }),
            |this, account| {
                this.child(walletconnect_kv_row(
                    "Public account",
                    walletconnect_public_account_provenance_label(account, public_accounts),
                ))
            },
        )
        .child(walletconnect_kv_element_row(
            "Request expiry",
            div()
                .min_w(px(0.0))
                .text_align(gpui::TextAlign::Right)
                .text_color(rgb(expiry_color))
                .whitespace_normal()
                .child(SharedString::from(expiry)),
        ))
}

fn walletconnect_public_account_provenance_label(
    selected_account: Address,
    public_accounts: &[PublicAccountMetadata],
) -> String {
    public_accounts
        .iter()
        .find(|account| account.address == selected_account)
        .and_then(|account| {
            account
                .label
                .as_deref()
                .filter(|label| !label.trim().is_empty())
                .map(|_| public_account_walletconnect_label(account))
        })
        .unwrap_or_else(|| short_address(&selected_account))
}

fn walletconnect_provenance_dapp_row(name: &str) -> gpui::Div {
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
                .child("Dapp"),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .text_align(gpui::TextAlign::Right)
                .truncate()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(name.to_owned())),
        )
}

fn walletconnect_duration_label(seconds: u64) -> String {
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds:02}s")
    }
}
