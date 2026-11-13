use std::sync::Arc;

use alloy::primitives::U256;
use ui::controls::AppNoticeTone;

use crate::assets::WalletIconSource;
use crate::root::ui_helpers::network_name;

use super::summary_view::spend_authorization_recipient_display;

#[derive(Clone)]
pub(in crate::root) struct SpendAuthorizationSummary {
    pub(super) title: Arc<str>,
    pub(super) detail: Arc<str>,
    pub(super) confirm_label: Arc<str>,
    pub(super) context: Option<Arc<str>>,
    pub(super) notice: Option<(AppNoticeTone, Arc<str>)>,
    pub(super) cards: Option<[SpendAuthorizationCard; 2]>,
    pub(super) rows: Vec<SpendAuthorizationSummaryRow>,
    pub(super) compact_rows: bool,
    pub(super) warnings: Vec<Arc<str>>,
    pub(super) payload: Option<SpendAuthorizationPayload>,
    requires_explicit_review: bool,
    pub(super) steps: Option<SpendAuthorizationSteps>,
    pub(super) title_network: Option<u64>,
    pub(super) row_group: Option<SpendAuthorizationDetails>,
    pub(super) details: Option<SpendAuthorizationDetails>,
    pub(super) disclosure: Option<SpendAuthorizationDisclosure>,
}

impl SpendAuthorizationSummary {
    pub(in crate::root) fn new(
        title: impl Into<Arc<str>>,
        detail: impl Into<Arc<str>>,
        rows: Vec<SpendAuthorizationSummaryRow>,
    ) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
            confirm_label: "Authorize and continue".into(),
            context: None,
            notice: None,
            cards: None,
            rows,
            compact_rows: false,
            warnings: Vec::new(),
            payload: None,
            requires_explicit_review: false,
            steps: None,
            title_network: None,
            row_group: None,
            details: None,
            disclosure: None,
        }
    }

    pub(in crate::root) fn with_confirm_label(mut self, label: impl Into<Arc<str>>) -> Self {
        self.confirm_label = label.into();
        self
    }

    pub(in crate::root) fn with_context(mut self, context: impl Into<Arc<str>>) -> Self {
        self.context = Some(context.into());
        self
    }

    pub(in crate::root) fn with_notice(
        mut self,
        tone: AppNoticeTone,
        text: impl Into<Arc<str>>,
    ) -> Self {
        self.notice = Some((tone, text.into()));
        self
    }

    /// Two cards above the rows: what the spend gives up, then what it receives.
    pub(in crate::root) fn with_cards(
        mut self,
        sell: SpendAuthorizationCard,
        receive: SpendAuthorizationCard,
    ) -> Self {
        self.cards = Some([sell, receive]);
        self
    }

    /// Show each row on one line, its value right-aligned and its hint behind an info button.
    pub(in crate::root) const fn with_compact_rows(mut self) -> Self {
        self.compact_rows = true;
        self
    }

    /// An alert under the rows that names what the spend makes public. `summary` follows
    /// "Made public:" on its one line, and the hint's card holds the full text.
    pub(in crate::root) fn with_disclosure(
        mut self,
        summary: impl Into<Arc<str>>,
        hint: SpendAuthorizationHint,
    ) -> Self {
        self.disclosure = Some(SpendAuthorizationDisclosure {
            summary: summary.into(),
            hint,
        });
        self
    }

    pub(in crate::root) fn with_warnings(mut self, warnings: Vec<Arc<str>>) -> Self {
        self.warnings = warnings;
        self
    }

    /// A stepper under the title: the steps named by `labels`, of which `current`, counted
    /// from one, is the one this dialog authorizes. `hint` explains them.
    pub(in crate::root) fn with_steps<L: Into<SpendAuthorizationLabel>>(
        mut self,
        current: usize,
        labels: impl IntoIterator<Item = L>,
        hint: SpendAuthorizationHint,
    ) -> Self {
        self.steps = Some(SpendAuthorizationSteps::new(current, labels, hint));
        self
    }

    /// A small chip beside the title, naming the network `chain_id`.
    pub(in crate::root) const fn with_title_network(mut self, chain_id: u64) -> Self {
        self.title_network = Some(chain_id);
        self
    }

    /// A collapsed disclosure under the rows, laid out as one more compact row.
    pub(in crate::root) fn with_details<L, V>(
        mut self,
        title: impl Into<Arc<str>>,
        collapsed_summary: impl Into<Arc<str>>,
        rows: Vec<(L, V)>,
        note: Option<&str>,
    ) -> Self
    where
        L: Into<Arc<str>>,
        V: Into<Arc<str>>,
    {
        self.details = Some(SpendAuthorizationDetails {
            title: title.into(),
            collapsed_summary: collapsed_summary.into(),
            content: SpendAuthorizationDetailsContent::Lines {
                rows: rows
                    .into_iter()
                    .map(|(label, value)| (label.into(), value.into()))
                    .collect(),
                note: note.map(Arc::from),
            },
        });
        self
    }

    /// A collapsed disclosure above the rows, laid out like [`Self::with_details`]. Open, it
    /// shows `rows` as compact rows in place of `collapsed_summary`. Fewer than two rows aren't
    /// grouped: they lead the summary's rows.
    pub(in crate::root) fn with_row_group(
        mut self,
        title: impl Into<Arc<str>>,
        collapsed_summary: impl Into<Arc<str>>,
        rows: Vec<SpendAuthorizationSummaryRow>,
    ) -> Self {
        if rows.len() < 2 {
            self.rows.splice(0..0, rows);
            return self;
        }
        self.row_group = Some(SpendAuthorizationDetails {
            title: title.into(),
            collapsed_summary: collapsed_summary.into(),
            content: SpendAuthorizationDetailsContent::Rows(rows),
        });
        self
    }

    /// The row group's rows, which the summary's rows follow.
    pub(super) fn grouped_rows(&self) -> &[SpendAuthorizationSummaryRow] {
        match self.row_group.as_ref().map(|group| &group.content) {
            Some(SpendAuthorizationDetailsContent::Rows(rows)) => rows,
            _ => &[],
        }
    }

    pub(in crate::root) fn with_payload(
        mut self,
        label: impl Into<Arc<str>>,
        value: impl Into<Arc<str>>,
    ) -> Self {
        self.payload = Some(SpendAuthorizationPayload {
            label: label.into(),
            value: value.into(),
        });
        self
    }

    pub(in crate::root) fn with_custom_transaction_fee(mut self, amount: String) -> Self {
        self.rows.push(SpendAuthorizationSummaryRow::new(
            "Custom transaction fee",
            amount,
        ));
        self.requires_explicit_review = true;
        self
    }

    pub(in crate::root) fn with_broadcaster_fee(
        mut self,
        estimated: String,
        maximum_label: &'static str,
        maximum: String,
    ) -> Self {
        self.rows.push(SpendAuthorizationSummaryRow::new(
            "Estimated transaction fee",
            estimated,
        ));
        self.rows
            .push(SpendAuthorizationSummaryRow::new(maximum_label, maximum));
        self
    }

    pub(in crate::root) const fn requiring_explicit_review(mut self) -> Self {
        self.requires_explicit_review = true;
        self
    }

    /// The row group's rows, then the rows outside it.
    #[cfg(test)]
    fn all_rows(&self) -> impl Iterator<Item = &SpendAuthorizationSummaryRow> {
        self.grouped_rows().iter().chain(&self.rows)
    }

    /// Each row's label and value, the row group's rows first. A copyable account's value
    /// follows its prefix, in full.
    #[cfg(test)]
    pub(in crate::root) fn rows_for_test(&self) -> Vec<(String, String)> {
        self.all_rows()
            .map(SpendAuthorizationSummaryRow::values_for_test)
            .collect()
    }

    /// The row group's title, its collapsed summary and its rows' labels.
    #[cfg(test)]
    pub(in crate::root) fn row_group_for_test(&self) -> Option<(String, String, Vec<String>)> {
        self.row_group.as_ref().map(|group| {
            (
                group.title.to_string(),
                group.collapsed_summary.to_string(),
                self.grouped_rows()
                    .iter()
                    .map(|row| row.label.to_string())
                    .collect(),
            )
        })
    }

    /// Each row's label and what the review shows for it, the row group's rows first: a
    /// copyable address is shortened after its prefix.
    #[cfg(test)]
    pub(in crate::root) fn shown_rows_for_test(&self) -> Vec<(String, String)> {
        self.all_rows()
            .map(|row| {
                (
                    row.label.to_string(),
                    if row.shortened_copyable {
                        row.shortened_value()
                    } else {
                        row.value.to_string()
                    },
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub(in crate::root) fn warnings_for_test(&self) -> Vec<String> {
        self.warnings.iter().map(ToString::to_string).collect()
    }

    #[cfg(test)]
    pub(in crate::root) fn steps_for_test(&self) -> Option<(usize, Vec<String>)> {
        self.steps.as_ref().map(|steps| {
            (
                steps.current,
                steps
                    .labels
                    .iter()
                    .map(SpendAuthorizationLabel::text)
                    .collect(),
            )
        })
    }

    #[cfg(test)]
    pub(in crate::root) fn details_for_test(&self) -> Vec<(String, String)> {
        match self.details.as_ref().map(|details| &details.content) {
            Some(SpendAuthorizationDetailsContent::Lines { rows, .. }) => rows
                .iter()
                .map(|(label, value)| (label.to_string(), value.to_string()))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The details disclosure's note.
    #[cfg(test)]
    pub(in crate::root) fn details_note_for_test(&self) -> Option<String> {
        match self.details.as_ref().map(|details| &details.content) {
            Some(SpendAuthorizationDetailsContent::Lines { note, .. }) => {
                note.as_deref().map(str::to_owned)
            }
            _ => None,
        }
    }

    /// The text of the hint behind the info button of the row labelled `label`.
    #[cfg(test)]
    pub(in crate::root) fn row_hint_for_test(&self, label: &str) -> Option<String> {
        self.all_rows()
            .find(|row| row.label.as_ref() == label)
            .and_then(|row| row.hint.as_ref())
            .map(SpendAuthorizationHint::text_for_test)
    }

    /// The disclosure alert's summary, its card's text, and whether it warns.
    #[cfg(test)]
    pub(in crate::root) fn disclosure_for_test(&self) -> Option<(String, String, bool)> {
        self.disclosure.as_ref().map(|disclosure| {
            (
                disclosure.summary.to_string(),
                disclosure.hint.text_for_test(),
                disclosure.hint.warning.is_some(),
            )
        })
    }

    /// The Send card's label and amount.
    #[cfg(test)]
    pub(in crate::root) fn send_card_for_test(&self) -> Option<(String, String)> {
        self.cards
            .as_ref()
            .map(|[send, _]| (send.label.text(), send.amount.to_string()))
    }

    /// The Receive card's USD value.
    #[cfg(test)]
    pub(in crate::root) fn receive_usd_for_test(&self) -> Option<String> {
        self.cards
            .as_ref()
            .and_then(|[_, receive]| receive.usd.as_deref().map(str::to_owned))
    }

    /// The networks whose badges the Send and Receive cards' icons carry.
    #[cfg(test)]
    pub(in crate::root) fn card_networks_for_test(&self) -> Option<[Option<u64>; 2]> {
        self.cards
            .as_ref()
            .map(|cards| cards.each_ref().map(|card| card.network))
    }

    /// The Receive card's label, amount and text lines.
    #[cfg(test)]
    pub(in crate::root) fn receive_card_for_test(&self) -> Option<(String, String, Vec<String>)> {
        self.cards.as_ref().map(|[_, receive]| {
            (
                receive.label.text(),
                receive.amount.to_string(),
                receive
                    .lines
                    .iter()
                    .filter_map(|line| match line {
                        SpendAuthorizationCardLine::Text {
                            before,
                            strong,
                            after,
                        } => Some(
                            [before, strong, after]
                                .into_iter()
                                .filter(|part| !part.is_empty())
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        SpendAuthorizationCardLine::Receiver { .. }
                        | SpendAuthorizationCardLine::Account { .. } => None,
                    })
                    .collect(),
            )
        })
    }

    /// The name and the address of the Send card's account line.
    #[cfg(test)]
    pub(in crate::root) fn send_account_for_test(&self) -> Option<(String, String)> {
        self.cards.as_ref().and_then(|[send, _]| {
            send.lines.iter().find_map(|line| match line {
                SpendAuthorizationCardLine::Account { name, address } => {
                    Some((name.to_string(), address.to_string()))
                }
                _ => None,
            })
        })
    }

    #[cfg(test)]
    pub(in crate::root) fn title_for_test(&self) -> (String, Option<String>) {
        (self.title.to_string(), self.title_network.map(network_name))
    }

    #[cfg(test)]
    pub(in crate::root) const fn compact_rows_for_test(&self) -> bool {
        self.compact_rows
    }

    /// The Sell and Receive cards' amount changes, each with whether it is adverse.
    #[cfg(test)]
    pub(in crate::root) fn card_deltas_for_test(&self) -> [Option<(String, bool)>; 2] {
        self.cards.as_ref().map_or([None, None], |cards| {
            cards.each_ref().map(|card| {
                card.delta
                    .as_ref()
                    .map(|delta| (delta.text.clone(), delta.adverse))
            })
        })
    }

    /// The amount change of the row labelled `label`, and whether it is adverse.
    #[cfg(test)]
    pub(in crate::root) fn row_delta_for_test(&self, label: &str) -> Option<(String, bool)> {
        self.all_rows()
            .find(|row| row.label.as_ref() == label)
            .and_then(|row| row.delta.as_ref())
            .map(|delta| (delta.text.clone(), delta.adverse))
    }

    /// The address the Receive card's receiver line copies and shows on hover, and the label
    /// the hover shows before it.
    #[cfg(test)]
    pub(in crate::root) fn receiver_for_test(&self) -> Option<(String, Option<String>)> {
        self.cards.as_ref().and_then(|[_, receive]| {
            receive.lines.iter().find_map(|line| match line {
                SpendAuthorizationCardLine::Receiver { address, label, .. } => {
                    Some((address.to_string(), label.as_deref().map(str::to_owned)))
                }
                SpendAuthorizationCardLine::Text { .. }
                | SpendAuthorizationCardLine::Account { .. } => None,
            })
        })
    }
}

/// An explanation behind an info button: a card of paragraphs under a title, shown on hover
/// and, on click or from the keyboard, in a popover.
#[derive(Clone)]
pub(in crate::root) struct SpendAuthorizationHint {
    pub(super) title: Arc<str>,
    pub(super) paragraphs: Vec<Arc<str>>,
    pub(super) warning: Option<Arc<str>>,
    pub(super) fact: Option<(Arc<str>, Arc<str>)>,
}

impl SpendAuthorizationHint {
    pub(in crate::root) fn new<P: Into<Arc<str>>>(
        title: impl Into<Arc<str>>,
        paragraphs: impl IntoIterator<Item = P>,
    ) -> Self {
        Self {
            title: title.into(),
            paragraphs: paragraphs.into_iter().map(Into::into).collect(),
            warning: None,
            fact: None,
        }
    }

    /// A last paragraph in the warning colour, which the title takes too.
    pub(in crate::root) fn with_warning(mut self, warning: impl Into<Arc<str>>) -> Self {
        self.warning = Some(warning.into());
        self
    }

    /// A label and its value on a line under the paragraphs.
    pub(in crate::root) fn with_fact(
        mut self,
        label: impl Into<Arc<str>>,
        value: impl Into<Arc<str>>,
    ) -> Self {
        self.fact = Some((label.into(), value.into()));
        self
    }

    /// The card's text below its title, one paragraph per line.
    #[cfg(test)]
    fn text_for_test(&self) -> String {
        self.paragraphs
            .iter()
            .chain(&self.warning)
            .map(ToString::to_string)
            .chain(
                self.fact
                    .iter()
                    .map(|(label, value)| format!("{label} {value}")),
            )
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A stepper's steps: see [`SpendAuthorizationSummary::with_steps`].
#[derive(Clone)]
pub(in crate::root) struct SpendAuthorizationSteps {
    pub(super) current: usize,
    pub(super) labels: Vec<SpendAuthorizationLabel>,
    pub(super) hint: SpendAuthorizationHint,
}

impl SpendAuthorizationSteps {
    pub(in crate::root) fn new<L: Into<SpendAuthorizationLabel>>(
        current: usize,
        labels: impl IntoIterator<Item = L>,
        hint: SpendAuthorizationHint,
    ) -> Self {
        Self {
            current,
            labels: labels.into_iter().map(Into::into).collect(),
            hint,
        }
    }
}

#[derive(Clone)]
pub(super) struct SpendAuthorizationDisclosure {
    pub(super) summary: Arc<str>,
    pub(super) hint: SpendAuthorizationHint,
}

#[derive(Clone)]
pub(super) struct SpendAuthorizationDetails {
    pub(super) title: Arc<str>,
    pub(super) collapsed_summary: Arc<str>,
    pub(super) content: SpendAuthorizationDetailsContent,
}

/// What an open details disclosure shows under its line.
#[derive(Clone)]
pub(super) enum SpendAuthorizationDetailsContent {
    /// Plain label and value lines, then a muted note.
    Lines {
        rows: Vec<(Arc<str>, Arc<str>)>,
        note: Option<Arc<str>>,
    },
    /// Compact rows, each with its info button.
    Rows(Vec<SpendAuthorizationSummaryRow>),
}

/// A card's or a step's label, or a row's label or value. One that names a network shows the
/// network's mark and name between `prefix` and `suffix`; any other is `prefix` alone.
#[derive(Clone)]
pub(in crate::root) struct SpendAuthorizationLabel {
    pub(super) prefix: Arc<str>,
    pub(super) network: Option<u64>,
    pub(super) suffix: Arc<str>,
}

impl SpendAuthorizationLabel {
    /// `prefix`, a space, the network `chain_id`, then `suffix` as it is written, such as
    /// ", exactly".
    pub(in crate::root) fn on_network(
        prefix: impl Into<Arc<str>>,
        chain_id: u64,
        suffix: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            network: Some(chain_id),
            suffix: suffix.into(),
        }
    }

    /// The label as plain text, with the network's name in place.
    fn text(&self) -> String {
        match self.network {
            Some(chain_id) => {
                let name = network_name(chain_id);
                format!("{} {name}{}", self.prefix, self.suffix)
            }
            None => self.prefix.to_string(),
        }
    }
}

impl From<&str> for SpendAuthorizationLabel {
    fn from(text: &str) -> Self {
        Self {
            prefix: text.into(),
            network: None,
            suffix: "".into(),
        }
    }
}

impl From<String> for SpendAuthorizationLabel {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

/// An amount card of a review: a small label, the token's icon and amount, the amount's
/// change beside it, the amount's USD value at the right edge, then small lines.
#[derive(Clone)]
pub(in crate::root) struct SpendAuthorizationCard {
    pub(super) label: SpendAuthorizationLabel,
    pub(super) amount: Arc<str>,
    pub(super) delta: Option<SpendAuthorizationAmountDelta>,
    pub(super) icon: Option<WalletIconSource>,
    pub(super) network: Option<u64>,
    pub(super) usd: Option<Arc<str>>,
    pub(super) lines: Vec<SpendAuthorizationCardLine>,
}

#[derive(Clone)]
pub(super) enum SpendAuthorizationCardLine {
    /// `strong` stands out between `before` and `after`. Empty parts are left out.
    Text {
        before: Arc<str>,
        strong: Arc<str>,
        after: Arc<str>,
    },
    /// "to" and the `short` form of `address`, with a copy button for the full address.
    /// Hovering shows the full address, after `label` when it has one.
    Receiver {
        address: Arc<str>,
        short: Arc<str>,
        label: Option<Arc<str>>,
    },
    /// "from", the account's `name` and its shortened `address`, with a copy button for the
    /// full address, which hovering shows.
    Account { name: Arc<str>, address: Arc<str> },
}

impl SpendAuthorizationCard {
    pub(in crate::root) fn new(
        label: impl Into<SpendAuthorizationLabel>,
        amount: impl Into<Arc<str>>,
        icon: Option<WalletIconSource>,
    ) -> Self {
        Self {
            label: label.into(),
            amount: amount.into(),
            delta: None,
            icon,
            network: None,
            usd: None,
            lines: Vec::new(),
        }
    }

    /// The network `chain_id` as a badge on the card's icon.
    pub(in crate::root) const fn with_network(mut self, chain_id: u64) -> Self {
        self.network = Some(chain_id);
        self
    }

    /// The signed difference of the card's amount from `previous`, beside the amount.
    pub(in crate::root) fn with_amount_change(
        mut self,
        previous: Option<U256>,
        current: U256,
        higher_is_worse: bool,
        format_amount: impl FnOnce(U256) -> String,
    ) -> Self {
        self.delta = SpendAuthorizationAmountDelta::between(
            previous,
            current,
            higher_is_worse,
            format_amount,
        );
        self
    }

    pub(in crate::root) fn with_usd(mut self, usd: Option<String>) -> Self {
        self.usd = usd.map(Arc::from);
        self
    }

    pub(in crate::root) fn with_line(self, text: impl Into<Arc<str>>) -> Self {
        self.with_emphasis(text, "", "")
    }

    /// A line whose `strong` part stands out between `before` and `after`.
    pub(in crate::root) fn with_emphasis(
        mut self,
        before: impl Into<Arc<str>>,
        strong: impl Into<Arc<str>>,
        after: impl Into<Arc<str>>,
    ) -> Self {
        self.lines.push(SpendAuthorizationCardLine::Text {
            before: before.into(),
            strong: strong.into(),
            after: after.into(),
        });
        self
    }

    /// A line naming the receiver by its shortened `address`, which it copies and shows on
    /// hover in full. `label` is the wallet's name for the address, when it has one.
    pub(in crate::root) fn with_receiver(
        self,
        address: impl Into<Arc<str>>,
        label: Option<String>,
    ) -> Self {
        let address = address.into();
        let short = spend_authorization_recipient_display(&address);
        self.with_short_receiver(address, short, label)
    }

    /// [`Self::with_receiver`] showing `short` as the shortened address, such as a non-EVM
    /// receiver in the form the swap's order detail gives it.
    pub(in crate::root) fn with_short_receiver(
        mut self,
        address: impl Into<Arc<str>>,
        short: impl Into<Arc<str>>,
        label: Option<String>,
    ) -> Self {
        self.lines.push(SpendAuthorizationCardLine::Receiver {
            address: address.into(),
            short: short.into(),
            label: label.map(Arc::from),
        });
        self
    }

    /// A line naming the paying account: "from", its `name` and its shortened `address`,
    /// which it copies and shows on hover in full.
    pub(in crate::root) fn with_account(
        mut self,
        name: impl Into<Arc<str>>,
        address: impl Into<Arc<str>>,
    ) -> Self {
        self.lines.push(SpendAuthorizationCardLine::Account {
            name: name.into(),
            address: address.into(),
        });
        self
    }
}

pub(in crate::root) const fn spend_authorization_can_use_cached_password(
    summary: &SpendAuthorizationSummary,
) -> bool {
    !summary.requires_explicit_review
}

#[derive(Clone)]
pub(super) struct SpendAuthorizationPayload {
    pub(super) label: Arc<str>,
    pub(super) value: Arc<str>,
}

#[derive(Clone)]
pub(in crate::root) struct SpendAuthorizationSummaryRow {
    pub(super) label: Arc<str>,
    pub(super) value: Arc<str>,
    /// The label and the value of a row that names a network there, which a compact row shows
    /// with the network's mark. `label` and `value` hold their text.
    pub(super) label_network: Option<SpendAuthorizationLabel>,
    pub(super) value_network: Option<SpendAuthorizationLabel>,
    pub(super) icon_path: Option<WalletIconSource>,
    pub(super) shortened_copyable: bool,
    /// The shortened form a shortened, copyable value shows, when it isn't the default one.
    short: Option<Arc<str>>,
    /// What a shortened, copyable value follows and what its copy control calls it, such as an
    /// account's number and "stealth account address".
    copyable_account: Option<(Arc<str>, Arc<str>)>,
    pub(super) delta: Option<SpendAuthorizationAmountDelta>,
    pub(super) hint: Option<SpendAuthorizationHint>,
}

#[derive(Clone)]
pub(super) struct SpendAuthorizationAmountDelta {
    pub(super) text: String,
    pub(super) adverse: bool,
}

impl SpendAuthorizationAmountDelta {
    /// The signed difference of `current` from `previous`, formatted by `format_amount`. `None`
    /// without a previous amount or when the two are equal.
    fn between(
        previous: Option<U256>,
        current: U256,
        higher_is_worse: bool,
        format_amount: impl FnOnce(U256) -> String,
    ) -> Option<Self> {
        let (text, increased) = Self::signed(previous?, current, format_amount)?;
        Some(Self {
            text,
            adverse: increased == higher_is_worse,
        })
    }

    /// An amount's change with its change in USD after it, such as "+0.12 USDC · +$0.12".
    /// `amounts` are the previous and current amounts when both are in one token, and `values`
    /// their USD values when both have a price. Amounts in different tokens show the USD
    /// change alone, and amounts without a price the token change alone. `None` when neither
    /// is known or nothing changed.
    fn priced(
        amounts: Option<(U256, U256)>,
        values: Option<(U256, U256)>,
        higher_is_worse: bool,
        format_amount: impl FnOnce(U256) -> String,
        format_value: impl FnOnce(U256) -> String,
    ) -> Option<Self> {
        let amount =
            amounts.and_then(|(previous, current)| Self::signed(previous, current, format_amount));
        let value =
            values.and_then(|(previous, current)| Self::signed(previous, current, format_value));
        let increased = amount.as_ref().or(value.as_ref())?.1;
        Some(Self {
            adverse: increased == higher_is_worse,
            text: [amount, value]
                .into_iter()
                .flatten()
                .map(|(text, _)| text)
                .collect::<Vec<_>>()
                .join(" · "),
        })
    }

    /// The signed difference of `current` from `previous`, formatted by `format`, and whether
    /// it is an increase. `None` when the two are equal.
    fn signed(
        previous: U256,
        current: U256,
        format: impl FnOnce(U256) -> String,
    ) -> Option<(String, bool)> {
        if previous == current {
            return None;
        }
        let increased = current > previous;
        let (sign, magnitude) = if increased {
            ("+", current - previous)
        } else {
            ("−", previous - current)
        };
        Some((format!("{sign}{}", format(magnitude)), increased))
    }
}

impl SpendAuthorizationSummaryRow {
    pub(in crate::root) fn new(label: impl Into<Arc<str>>, value: impl Into<Arc<str>>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            label_network: None,
            value_network: None,
            icon_path: None,
            shortened_copyable: false,
            short: None,
            copyable_account: None,
            delta: None,
            hint: None,
        }
    }

    /// A row whose label or value may name a network, which then carries the network's mark.
    /// Shown for compact rows.
    pub(in crate::root) fn naming_network(
        label: impl Into<SpendAuthorizationLabel>,
        value: impl Into<SpendAuthorizationLabel>,
    ) -> Self {
        let (label, value): (SpendAuthorizationLabel, SpendAuthorizationLabel) =
            (label.into(), value.into());
        let mut row = Self::new(label.text(), value.text());
        row.label_network = label.network.is_some().then_some(label);
        row.value_network = value.network.is_some().then_some(value);
        row
    }

    /// An explanation behind an info button at the row's end. Shown for compact rows.
    pub(in crate::root) fn with_hint(mut self, hint: SpendAuthorizationHint) -> Self {
        self.hint = Some(hint);
        self
    }

    pub(in crate::root) fn with_icon(mut self, icon_path: Option<WalletIconSource>) -> Self {
        self.icon_path = icon_path;
        self
    }

    pub(in crate::root) const fn with_shortened_copyable(mut self) -> Self {
        self.shortened_copyable = true;
        self
    }

    /// [`Self::with_shortened_copyable`] showing `short` as the shortened value, such as a
    /// non-EVM mint in the form the swap's form and order detail give it.
    pub(in crate::root) fn with_short_copyable(mut self, short: impl Into<Arc<str>>) -> Self {
        self.shortened_copyable = true;
        self.short = Some(short.into());
        self
    }

    /// The value is an account's address: shortened after `prefix`, such as "#35 · ", with a
    /// control that copies it in full and is named `name`.
    pub(in crate::root) fn with_copyable_account(
        mut self,
        prefix: impl Into<Arc<str>>,
        name: impl Into<Arc<str>>,
    ) -> Self {
        self.shortened_copyable = true;
        self.copyable_account = Some((prefix.into(), name.into()));
        self
    }

    /// What a shortened, copyable row shows: its value, shortened, after its prefix.
    pub(super) fn shortened_value(&self) -> String {
        let short = self.short.as_deref().map_or_else(
            || spend_authorization_recipient_display(self.value.as_ref()),
            str::to_owned,
        );
        match &self.copyable_account {
            Some((prefix, _)) => format!("{prefix}{short}"),
            None => short,
        }
    }

    /// The tooltip of a shortened, copyable row's copy control.
    pub(super) fn copy_tooltip(&self) -> String {
        match &self.copyable_account {
            Some((_, name)) => format!("Copy {name}"),
            None => format!("Copy {}", self.label.to_ascii_lowercase()),
        }
    }

    pub(in crate::root) fn with_amount_change(
        mut self,
        previous: Option<U256>,
        current: U256,
        higher_is_worse: bool,
        format_amount: impl FnOnce(U256) -> String,
    ) -> Self {
        self.delta = SpendAuthorizationAmountDelta::between(
            previous,
            current,
            higher_is_worse,
            format_amount,
        );
        self
    }

    /// [`Self::with_amount_change`] with the change in USD after it, formatted by
    /// `format_value`. `amounts` are the previous and current amounts when both are in one
    /// token, and `values` their USD values when both have a price.
    pub(in crate::root) fn with_priced_amount_change(
        mut self,
        amounts: Option<(U256, U256)>,
        values: Option<(U256, U256)>,
        higher_is_worse: bool,
        format_amount: impl FnOnce(U256) -> String,
        format_value: impl FnOnce(U256) -> String,
    ) -> Self {
        self.delta = SpendAuthorizationAmountDelta::priced(
            amounts,
            values,
            higher_is_worse,
            format_amount,
            format_value,
        );
        self
    }

    #[cfg(test)]
    pub(in crate::root) fn values_for_test(&self) -> (String, String) {
        let prefix = self
            .copyable_account
            .as_ref()
            .map_or("", |(prefix, _)| prefix.as_ref());
        (self.label.to_string(), format!("{prefix}{}", self.value))
    }
}
