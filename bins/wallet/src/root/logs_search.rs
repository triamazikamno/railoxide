//! The logs panel's search: `level:`, `target:` and `hide:` filters over the retained log
//! lines, shown by the shared [`SearchField`](super::search_field::SearchField).

use std::collections::BTreeSet;

use tracing::Level;
use ui::logs::{LogEntry, LogFilter, LogStore, format_count, target_matches};

use crate::assets::WalletIconSource;
use crate::root::search_field::{
    ChipCombine, SearchFilter, SearchSource, Suggestion, SuggestionAction, suggestion,
};

/// `level:` values: each shows its level and the more severe ones.
const LEVELS: [(Level, &str); 5] = [
    (Level::ERROR, "Error"),
    (Level::WARN, "Warn and above"),
    (Level::INFO, "Info and above"),
    (Level::DEBUG, "Debug and above"),
    (Level::TRACE, "Trace and above"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LogFacet {
    Level,
    Target,
    Hide,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LogChip {
    /// Lines at this level or more severe.
    Level(Level),
    /// Lines from this module or its submodules.
    Target(String),
    /// Leaves out lines from this module and its submodules.
    Hide(String),
}

/// The retained lines, which the search suggests modules from.
pub(super) struct LogsSearchSource {
    pub(super) store: LogStore,
}

impl SearchSource for LogsSearchSource {
    type Facet = LogFacet;
    type Chip = LogChip;
    const ID_PREFIX: &'static str = "wallet-logs";
    const PLACEHOLDER: &'static str = "Search logs";
    const FACETS: &'static [LogFacet] = &[LogFacet::Level, LogFacet::Target, LogFacet::Hide];

    fn keyword(facet: LogFacet) -> &'static str {
        match facet {
            LogFacet::Level => "level:",
            LogFacet::Target => "target:",
            LogFacet::Hide => "hide:",
        }
    }

    fn title(facet: LogFacet) -> &'static str {
        match facet {
            LogFacet::Level => "Level",
            LogFacet::Target => "Target",
            LogFacet::Hide => "Hide target",
        }
    }

    fn hint(facet: LogFacet) -> &'static str {
        match facet {
            LogFacet::Level => "error, warn and above…",
            LogFacet::Target | LogFacet::Hide => "module, such as arti_client",
        }
    }

    fn combine(facet: LogFacet) -> ChipCombine {
        if facet == LogFacet::Hide {
            ChipCombine::All
        } else {
            ChipCombine::Any
        }
    }

    fn chip_facet(chip: &LogChip) -> LogFacet {
        match chip {
            LogChip::Level(_) => LogFacet::Level,
            LogChip::Target(_) => LogFacet::Target,
            LogChip::Hide(_) => LogFacet::Hide,
        }
    }

    fn chip_value(&self, chip: &LogChip) -> (String, Option<WalletIconSource>) {
        let value = match chip {
            LogChip::Level(level) => LEVELS
                .iter()
                .find(|(value, _)| value == level)
                .map_or_else(|| level.to_string(), |(_, label)| (*label).to_owned()),
            LogChip::Target(module) | LogChip::Hide(module) => module.clone(),
        };
        (value, None)
    }

    fn facet_values(
        &self,
        facet: LogFacet,
        query: &str,
        limit: usize,
    ) -> Vec<Suggestion<LogFacet, LogChip>> {
        if facet == LogFacet::Level {
            return LEVELS
                .iter()
                .filter(|(_, label)| label.to_lowercase().contains(query))
                .take(limit)
                .map(|&(level, label)| {
                    suggestion(
                        label.to_owned(),
                        None,
                        SuggestionAction::AddChip(LogChip::Level(level)),
                    )
                })
                .collect();
        }
        let logs = self.store.logs();
        modules(&logs, query)
            .into_iter()
            .take(limit)
            .map(|module| {
                let count = logs
                    .iter()
                    .filter(|entry| target_matches(&entry.target, module))
                    .count();
                let chip = if facet == LogFacet::Target {
                    LogChip::Target(module.to_owned())
                } else {
                    LogChip::Hide(module.to_owned())
                };
                Suggestion {
                    note: Some(line_count(count)),
                    ..suggestion(module.to_owned(), None, SuggestionAction::AddChip(chip))
                }
            })
            .collect()
    }

    /// Free text lists the modules that hold it, to show or to hide.
    fn text_facets(&self, _query: &str) -> Vec<LogFacet> {
        vec![LogFacet::Target, LogFacet::Hide]
    }
}

/// Every retained target and each of its parent modules that holds `query`, lowercased.
/// Sorted by path, so a parent precedes its children.
fn modules<'a>(logs: &'a [LogEntry], query: &str) -> BTreeSet<&'a str> {
    let mut modules = BTreeSet::new();
    for entry in logs {
        let target = entry.target.as_ref();
        modules.extend(target.match_indices("::").map(|(end, _)| &target[..end]));
        modules.insert(target);
    }
    modules.retain(|module| module.to_lowercase().contains(query));
    modules
}

/// "1 line", "4,812 lines".
pub(super) fn line_count(n: usize) -> String {
    format!(
        "{} {}",
        format_count(n),
        if n == 1 { "line" } else { "lines" }
    )
}

/// The filter the logs pane applies for the field's chips and free text. Several `level:`
/// chips pass the most verbose of their levels.
pub(super) fn log_filter(filter: &SearchFilter<LogChip>) -> LogFilter {
    let mut min_level = None;
    let mut targets = Vec::new();
    let mut hides = Vec::new();
    for chip in &filter.chips {
        match chip {
            // tracing orders a more verbose level as greater.
            LogChip::Level(level) => min_level = min_level.max(Some(*level)),
            LogChip::Target(module) => targets.push(module.clone()),
            LogChip::Hide(module) => hides.push(module.clone()),
        }
    }
    LogFilter::new(filter.query(), min_level, targets, hides)
}

/// A row's Show only: replaces every `target:` chip with one for `target`.
pub(super) fn show_only(chips: &mut Vec<LogChip>, target: &str) {
    chips.retain(|chip| !matches!(chip, LogChip::Target(_)));
    chips.push(LogChip::Target(target.to_owned()));
}

/// A row's Hide: adds a `hide:` chip for `target`.
pub(super) fn hide(chips: &mut Vec<LogChip>, target: &str) {
    let chip = LogChip::Hide(target.to_owned());
    if !chips.contains(&chip) {
        chips.push(chip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(level: Level, target: &str) -> LogEntry {
        LogEntry {
            seq: 0,
            unix_ms: 0,
            level,
            target: target.into(),
            message: "message".into(),
        }
    }

    fn filter(chips: Vec<LogChip>) -> SearchFilter<LogChip> {
        let mut filter = SearchFilter::default();
        filter.chips = chips;
        filter
    }

    /// One chip's own match; the shared field combines them by facet.
    fn chip_matches(chip: &LogChip, entry: &LogEntry) -> bool {
        match chip {
            LogChip::Level(level) => entry.level <= *level,
            LogChip::Target(module) => target_matches(&entry.target, module),
            LogChip::Hide(module) => !target_matches(&entry.target, module),
        }
    }

    #[test]
    fn hide_chips_all_apply_through_the_field_and_the_pane_filter() {
        let filter = filter(vec![
            LogChip::Hide("arti_client".into()),
            LogChip::Hide("wallet_ops::poi".into()),
        ]);
        for (target, shown) in [
            ("arti_client::status", false),
            ("wallet_ops::poi::x", false),
            ("wallet_ops::rpc_broker", true),
        ] {
            let entry = line(Level::INFO, target);
            assert_eq!(
                filter.chips_match::<LogsSearchSource>(|chip| chip_matches(chip, &entry)),
                shown,
                "field: {target}"
            );
            assert_eq!(log_filter(&filter).matches(&entry), shown, "pane: {target}");
        }
    }

    #[test]
    fn module_values_offer_a_parent_that_never_logs_first() {
        let store = LogStore::new(10);
        for target in ["arti_client::client", "arti_client::status"] {
            store.push(line(Level::INFO, target));
        }
        let values = LogsSearchSource { store }.facet_values(LogFacet::Hide, "arti", 5);
        assert_eq!(values[0].title, "arti_client");
        assert_eq!(values[0].note.as_deref(), Some("2 lines"));
        assert_eq!(
            values[0].action,
            SuggestionAction::AddChip(LogChip::Hide("arti_client".into()))
        );
    }

    #[test]
    fn show_only_replaces_every_target_and_keeps_other_chips() {
        let mut chips = vec![
            LogChip::Target("wallet_ops".into()),
            LogChip::Level(Level::WARN),
        ];
        show_only(&mut chips, "wallet_ops::rpc_broker");
        assert_eq!(
            chips,
            [
                LogChip::Level(Level::WARN),
                LogChip::Target("wallet_ops::rpc_broker".into()),
            ]
        );
        let filter = log_filter(&filter(chips));
        assert!(filter.matches(&line(Level::WARN, "wallet_ops::rpc_broker")));
        assert!(!filter.matches(&line(Level::WARN, "wallet_ops::poi")));
        assert!(!filter.matches(&line(Level::INFO, "wallet_ops::rpc_broker")));
    }
}
