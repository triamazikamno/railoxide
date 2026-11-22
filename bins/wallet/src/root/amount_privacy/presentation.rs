//! The words of the amount privacy hints: wallet-ops' evaluation as lines, common amounts, a
//! notice and one sentence per factor, ready for the component to render or the review to keep.

use std::ops::Range;

use alloy::primitives::U256;
use wallet_ops::{
    CommonAmount, NetworkRoute, PoolShare, PrivacyFactor, PrivacyFactorKind, PrivacyLevel,
    ShieldHints, StatisticsNotice, TreeDeposits, UnshieldHints, UnshieldSender,
};

/// The info button on a factor that names a tree: its accessible label, then its popover's
/// title and text.
pub(super) const TREES_INFO_LABEL: &str = "About trees";
pub(super) const TREES_INFO_TITLE: &str = "Trees";
pub(super) const TREES_INFO_TEXT: &str = "RAILGUN keeps deposits in numbered trees of about 65,000 notes each. New deposits go into the current tree until it fills up, then the next tree starts. When a note is spent, the transaction shows which tree it came from, but not which note in that tree. The other deposits of the same token in that tree are the crowd your note blends into: the more there are, the harder it is to link a spend back to its deposit.";
/// The chart button on the amount-crowd factor: its accessible label, then its popover's footer,
/// which Unshield's common exit amounts share when they can be chosen.
pub(super) const COMMON_AMOUNTS_LABEL: &str = "Show common amounts";
pub(super) const COMMON_AMOUNTS_FOOTER: &str = "Click an amount to use it.";
/// The footer of Unshield's common exit amounts when fees are added on top, so none can be chosen.
pub(super) const COMMON_EXIT_AMOUNTS_BROWSE_FOOTER: &str =
    "Deduct fees from the amount to choose one of these.";
const COMMON_EXIT_AMOUNTS_LABEL: &str = "Show common exit amounts";
const COMMON_EXIT_AMOUNTS_EXPLAINER: &str = "Exits of the same amount blend in with each other. An uncommon amount is fine as long as it isn't close to one of your deposits.";

/// What the hints component shows, as text and levels, so the review can keep a copy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::root) struct AmountPrivacyPresentation {
    /// Figures that are not factors, one item each.
    pub(in crate::root) lines: Vec<String>,
    /// The amounts near the entered one that many deposits used, which Shield's amount-crowd
    /// factor offers in a popover, or that many recent exits used, which Unshield offers after
    /// its factors.
    pub(in crate::root) common_amounts: Option<CommonAmounts>,
    /// Why the counts are missing.
    pub(in crate::root) notice: Option<&'static str>,
    /// Bad first, then caution, then good, as wallet-ops orders them.
    pub(in crate::root) factors: Vec<PrivacyFactorLine>,
}

impl AmountPrivacyPresentation {
    /// The notice and the factors, which the review repeats.
    pub(in crate::root) fn factors_only(self) -> Self {
        Self {
            notice: self.notice,
            factors: self.factors,
            ..Self::default()
        }
    }
}

/// The common amounts popover: its title, such as "Common USDC amounts", the labelled button
/// that opens it, any explainer under the title, and its rows, ascending with the entered amount
/// in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct CommonAmounts {
    pub(in crate::root) title: String,
    pub(in crate::root) button_label: &'static str,
    pub(in crate::root) explainer: Option<&'static str>,
    pub(in crate::root) rows: Vec<CommonAmountRow>,
    /// Whether a row can be chosen to fill in the amount. Shield's always can.
    pub(in crate::root) choosable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct CommonAmountRow {
    /// The amount without its symbol, followed by " · yours" on the entered one.
    pub(in crate::root) amount_text: String,
    /// The deposits or exits, grouped in thousands.
    pub(in crate::root) count_text: String,
    /// What assistive technology reads, such as "0.01 ETH, 1,830 deposits in total".
    pub(in crate::root) accessibility_label: String,
    pub(in crate::root) count: u64,
    /// The amount to type: before the shield fee on Shield, the exit's value on Unshield.
    pub(in crate::root) amount: U256,
    pub(in crate::root) entered: bool,
    /// The amount can be entered in full, which choosing it needs.
    pub(in crate::root) within_balance: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct PrivacyFactorLine {
    pub(in crate::root) level: PrivacyLevel,
    /// Names the factor's kind, which a list holds once, for its element id.
    pub(in crate::root) key: &'static str,
    pub(in crate::root) text: String,
    /// Byte ranges of `text` shown in bold.
    pub(in crate::root) emphasis: Vec<Range<usize>>,
    /// The line names a tree, so it offers the explanation of trees.
    pub(in crate::root) explains_trees: bool,
}

impl PrivacyFactorLine {
    /// What assistive technology reads: the level, then the line.
    pub(in crate::root) fn accessibility_label(&self) -> String {
        format!("{}: {}", level_name(self.level), self.text)
    }
}

/// Amounts show with `symbol`, or in raw units when `decimals` are unknown.
pub(in crate::root) fn shield_presentation(
    hints: &ShieldHints,
    decimals: Option<u8>,
    symbol: &str,
) -> AmountPrivacyPresentation {
    let format_amount = |amount| token_amount(amount, decimals, symbol);
    let mut presentation = AmountPrivacyPresentation {
        factors: factor_lines(&hints.factors, 1, &format_amount),
        ..AmountPrivacyPresentation::default()
    };
    let lines = match &hints.lines {
        Ok(lines) => lines,
        Err(notice) => {
            presentation.notice = Some(notice_text(*notice));
            return presentation;
        }
    };
    presentation.common_amounts = common_amounts(&lines.common_amounts, decimals, symbol, false);
    if !lines.repeated_amounts {
        presentation
            .lines
            .push("No amount of this token has been used twice.".to_owned());
    }
    presentation
}

/// The popover, `None` when wallet-ops lists no common amount. `exits` gives it Unshield's words
/// for exits in the last 30 days in place of Shield's for all-time deposits.
fn common_amounts(
    amounts: &[CommonAmount],
    decimals: Option<u8>,
    symbol: &str,
    exits: bool,
) -> Option<CommonAmounts> {
    let (title, button_label, noun, period) = if exits {
        let title = format!("Common {symbol} exits in the last 30 days");
        (
            title,
            COMMON_EXIT_AMOUNTS_LABEL,
            "exit",
            "in the last 30 days",
        )
    } else {
        let title = format!("Common {symbol} amounts");
        (title, COMMON_AMOUNTS_LABEL, "deposit", "in total")
    };
    let row = |common: &CommonAmount| {
        let bare = decimals.map_or_else(
            || common.amount.to_string(),
            |decimals| railgun_ui::format_token_amount(common.amount, decimals),
        );
        CommonAmountRow {
            amount_text: if common.entered {
                format!("{bare} · yours")
            } else {
                bare
            },
            count_text: count(common.count),
            accessibility_label: format!(
                "{}, {} {period}",
                token_amount(common.amount, decimals, symbol),
                counted(common.count, noun)
            ),
            count: common.count,
            amount: common.amount,
            entered: common.entered,
            within_balance: common.within_balance,
        }
    };
    (!amounts.is_empty()).then(|| CommonAmounts {
        title,
        button_label,
        explainer: exits.then_some(COMMON_EXIT_AMOUNTS_EXPLAINER),
        rows: amounts.iter().map(row).collect(),
        choosable: true,
    })
}

/// Amounts show with `symbol`, or in raw units when `decimals` are unknown. The common exit
/// amounts can be chosen only when `choosable`.
pub(in crate::root) fn unshield_presentation(
    hints: &UnshieldHints,
    decimals: Option<u8>,
    symbol: &str,
    choosable: bool,
) -> AmountPrivacyPresentation {
    let format_amount = |amount| token_amount(amount, decimals, symbol);
    let (lines, notice) = match &hints.lines {
        Ok(lines) => (lines.as_ref(), None),
        Err(notice) => (None, Some(notice_text(*notice))),
    };
    let mut presentation = AmountPrivacyPresentation {
        notice,
        factors: factor_lines(
            &hints.factors,
            lines.map_or(1, |lines| lines.unshields.len()),
            &format_amount,
        ),
        ..AmountPrivacyPresentation::default()
    };
    let Some(lines) = lines else {
        return presentation;
    };
    presentation.common_amounts =
        common_amounts(&lines.common_exit_amounts, decimals, symbol, true);
    if let Some(common) = &mut presentation.common_amounts {
        common.choosable = choosable;
    }
    if lines.unshields.len() > 1 {
        presentation.lines.push(format!(
            "This exit is sent in one transaction as {} unshields.",
            lines.unshields.len()
        ));
    }
    presentation
}

fn factor_lines(
    factors: &[PrivacyFactor],
    unshields: usize,
    format_amount: &dyn Fn(U256) -> String,
) -> Vec<PrivacyFactorLine> {
    factors
        .iter()
        .map(|factor| {
            let (text, emphasis) = factor_text(factor.kind, unshields, format_amount);
            PrivacyFactorLine {
                level: factor.level,
                key: factor_key(factor.kind),
                text,
                emphasis,
                explains_trees: matches!(
                    factor.kind,
                    PrivacyFactorKind::CurrentTreeCrowd(_)
                        | PrivacyFactorKind::DirectExit(_)
                        | PrivacyFactorKind::FundingCrowd(_)
                ),
            }
        })
        .collect()
}

/// The amount-crowd factor's key, whose row offers the Shield common amounts.
pub(super) const AMOUNT_CROWD_KEY: &str = "amount-crowd";

const fn factor_key(kind: PrivacyFactorKind) -> &'static str {
    match kind {
        PrivacyFactorKind::AmountCrowd { .. } => AMOUNT_CROWD_KEY,
        PrivacyFactorKind::CurrentTreeCrowd(_) => "current-tree-crowd",
        PrivacyFactorKind::PoolShare(_) => "pool-share",
        PrivacyFactorKind::DirectExit(_) => "direct-exit",
        PrivacyFactorKind::FundingCrowd(_) => "funding-crowd",
        PrivacyFactorKind::RecentExits { .. } => "recent-exits",
        PrivacyFactorKind::CloseToOwnDeposit { .. } => "close-to-own-deposit",
        PrivacyFactorKind::QuietPeriod { .. } => "quiet-period",
        PrivacyFactorKind::RecentDeposit { .. } => "recent-deposit",
        PrivacyFactorKind::Sender(_) => "sender",
        PrivacyFactorKind::RecipientDeposited => "recipient-deposited",
        PrivacyFactorKind::Network(_) => "network",
    }
}

/// One line per factor, with the figures wallet-ops gives, and the byte ranges to emphasise.
/// `unshields` is how many unshields the exit is sent as.
fn factor_text(
    kind: PrivacyFactorKind,
    unshields: usize,
    format_amount: &dyn Fn(U256) -> String,
) -> (String, Vec<Range<usize>>) {
    let text = match kind {
        PrivacyFactorKind::AmountCrowd { deposits } => amount_crowd_text(deposits),
        PrivacyFactorKind::CurrentTreeCrowd(tree) => current_tree_crowd_text(tree),
        PrivacyFactorKind::PoolShare(Some(pool)) => return pool_share_text(pool, format_amount),
        PrivacyFactorKind::PoolShare(None) => "The pool holds none of this token yet.".to_owned(),
        PrivacyFactorKind::DirectExit(tree) => direct_exit_text(tree),
        PrivacyFactorKind::FundingCrowd(tree) => funding_crowd_text(tree),
        PrivacyFactorKind::RecentExits { exits_30d } => recent_exits_text(exits_30d, unshields),
        PrivacyFactorKind::CloseToOwnDeposit { deposit, others } => {
            close_to_own_deposit_text(&format_amount(deposit), others, unshields)
        }
        PrivacyFactorKind::QuietPeriod {
            exits_24h,
            exits_30d,
            suggest_waiting,
        } => quiet_period_text(exits_24h, exits_30d, suggest_waiting),
        PrivacyFactorKind::RecentDeposit { age_secs } => format!(
            "A note this exit spends came from a deposit made {} ago.",
            age_text(age_secs)
        ),
        PrivacyFactorKind::Sender(sender) => sender_text(sender).to_owned(),
        PrivacyFactorKind::RecipientDeposited => {
            "The recipient is one of your Public accounts and already deposited into the pool."
                .to_owned()
        }
        PrivacyFactorKind::Network(route) => network_text(route).to_owned(),
    };
    (text, Vec::new())
}

/// `None` stands for a count below two, which the statistics leave out.
fn amount_crowd_text(deposits: Option<u64>) -> String {
    match deposits {
        None => "At most one other deposit has used this amount.".to_owned(),
        Some(n) => format!(
            "{} in total {} used this amount.",
            counted(n, "deposit"),
            if n == 1 { "has" } else { "have" }
        ),
    }
}

fn current_tree_crowd_text(tree: TreeDeposits) -> String {
    match tree.deposits {
        0 => format!(
            "Your deposit joins tree #{}, which holds no other deposit of this token yet.",
            tree.tree
        ),
        deposits => format!(
            "Your deposit joins tree #{}, which holds {} of this token.",
            tree.tree,
            counted(deposits, "deposit")
        ),
    }
}

/// The pool's balance and the deposit's share of it, with both figures emphasised.
fn pool_share_text(
    pool: PoolShare,
    format_amount: &dyn Fn(U256) -> String,
) -> (String, Vec<Range<usize>>) {
    let (qualifier, percent) = share_text(pool.share_ppm);
    let parts = [
        ("The pool holds ".to_owned(), false),
        (format_amount(pool.balance), true),
        (format!(". This deposit is {qualifier} "), false),
        (percent, true),
        (" of it.".to_owned(), false),
    ];
    let mut text = String::new();
    let mut emphasis = Vec::new();
    for (part, emphasised) in parts {
        let start = text.len();
        text.push_str(&part);
        if emphasised {
            emphasis.push(start..text.len());
        }
    }
    (text, emphasis)
}

fn direct_exit_text(tree: TreeDeposits) -> String {
    let others = match tree.deposits {
        0 => "No other deposit".to_owned(),
        1 => "One other deposit".to_owned(),
        deposits => format!("{} other deposits", count(deposits)),
    };
    format!(
        "This exit spends one of your deposits in full. {others} in tree #{} {} that note's value.",
        tree.tree,
        if tree.deposits > 1 { "have" } else { "has" }
    )
}

fn funding_crowd_text(tree: TreeDeposits) -> String {
    match tree.deposits {
        0 => format!(
            "No deposit in tree #{} is clearly large enough to have funded this exit.",
            tree.tree
        ),
        deposits => format!(
            "At least {} in tree #{} {} large enough to have funded this exit.",
            counted(deposits, "deposit"),
            tree.tree,
            if deposits > 1 { "are" } else { "is" }
        ),
    }
}

fn recent_exits_text(exits_30d: u64, unshields: usize) -> String {
    if unshields > 1 {
        format!(
            "At least {} exits in the last 30 days used each of these amounts.",
            count(exits_30d)
        )
    } else {
        format!(
            "{} in the last 30 days used this amount.",
            counted(exits_30d, "exit")
        )
    }
}

fn close_to_own_deposit_text(deposit: &str, others: u64, unshields: usize) -> String {
    let subject = if unshields > 1 {
        "One of these unshields is"
    } else {
        "This exit is"
    };
    // The snapshot omits amounts only one deposit used, so the count is a lower bound.
    let others = match others {
        0 => "and no deposit by anyone else is known to be".to_owned(),
        1 => "but at least one deposit by someone else is".to_owned(),
        others => format!("but at least {} deposits by others are", count(others)),
    };
    format!("{subject} within 5% of your {deposit} deposit, {others} in that range.")
}

fn quiet_period_text(exits_24h: u64, exits_30d: u64, suggest_waiting: bool) -> String {
    if suggest_waiting {
        format!(
            "Only {} of this token in the last 24 hours, under half its usual pace. Waiting for more activity helps.",
            counted(exits_24h, "exit")
        )
    } else if exits_30d == 0 {
        "This token had no exits in the last 30 days.".to_owned()
    } else {
        format!(
            "This token had only {} in the last 30 days.",
            counted(exits_30d, "exit")
        )
    }
}

const fn sender_text(sender: UnshieldSender) -> &'static str {
    match sender {
        UnshieldSender::PublicAccount => {
            "Your Public account sends this transaction, so it appears publicly beside the recipient."
        }
        UnshieldSender::Broadcaster => {
            "A broadcaster sends this transaction, so none of your Public accounts appears as its sender."
        }
        // wallet-ops gives no sender factor for calldata the user submits elsewhere.
        UnshieldSender::CalldataOnly => "You submit this transaction from another wallet.",
    }
}

const fn network_text(route: NetworkRoute) -> &'static str {
    match route {
        NetworkRoute::Private => "RPC traffic for this chain goes through Tor or your proxy.",
        NetworkRoute::Direct => {
            "RPC traffic for this chain goes out directly, so the RPC provider sees your IP address."
        }
    }
}

const fn notice_text(notice: StatisticsNotice) -> &'static str {
    match notice {
        StatisticsNotice::NotConfigured => {
            "Amount statistics are not configured for this data source."
        }
        StatisticsNotice::NotYetAvailable => "Amount statistics are not available yet.",
        StatisticsNotice::OutOfDate => "Amount statistics are out of date.",
    }
}

const fn level_name(level: PrivacyLevel) -> &'static str {
    match level {
        PrivacyLevel::Bad => "Bad",
        PrivacyLevel::Caution => "Caution",
        PrivacyLevel::Good => "Good",
    }
}

fn count(n: u64) -> String {
    ui::logs::format_count(usize::try_from(n).unwrap_or(usize::MAX))
}

/// `n` and `noun`, plural unless `n` is one, such as "1,204 deposits".
fn counted(n: u64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{} {noun}s", count(n))
    }
}

/// A share in parts per million, rounded down to a hundredth of a percent, as a qualifier and
/// the percentage, such as ("about", "0.04%").
fn share_text(ppm: u64) -> (&'static str, String) {
    if ppm < 100 {
        return ("less than", "0.01%".to_owned());
    }
    (
        "about",
        format!("{}.{:02}%", ppm / 10_000, ppm % 10_000 / 100),
    )
}

/// An age under a day, to the minute or the hour.
fn age_text(secs: u64) -> String {
    let (n, unit) = match secs {
        0..60 => return "less than a minute".to_owned(),
        60..3_600 => (secs / 60, "minute"),
        _ => (secs / 3_600, "hour"),
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// An amount with its token's symbol, or raw units for a token of unknown decimals.
pub(super) fn token_amount(amount: U256, decimals: Option<u8>, symbol: &str) -> String {
    decimals.map_or_else(
        || format!("{amount} raw units"),
        |decimals| {
            format!(
                "{} {symbol}",
                railgun_ui::format_token_amount(amount, decimals)
            )
        },
    )
}
