#[allow(clippy::wildcard_imports)]
use super::*;

impl FeesDelegate {
    pub(crate) fn new(
        broadcaster_input: Entity<InputState>,
        chain_select: Entity<SelectState<Vec<FeesChainFilterItem>>>,
        initial_chain_filter: FeesFilter<u64>,
        token_select: Entity<SelectState<SearchableVec<FeesTokenFilterItem>>>,
        default_fee_tokens: Vec<(u64, Address)>,
        fee_anchor_lookup: FeeAnchorLookup,
    ) -> Self {
        let initial_token_filter =
            default_token_filter_for_chain(initial_chain_filter, default_fee_tokens.as_slice());
        Self {
            all_rows: Arc::from(Vec::<FeeRow>::new()),
            rows: Arc::from(Vec::<FeeRow>::new()),
            columns: fee_columns(false),
            fee_anchor_lookup,
            preference_hooks: None,
            chain_select,
            chain_filter: initial_chain_filter,
            broadcaster_query: Arc::from(""),
            broadcaster_input,
            token_filter: initial_token_filter,
            default_fee_tokens,
            token_select,
            synced_token_filter_items: Vec::new(),
            fee_sort: ColumnSort::Default,
            bonus_sort: ColumnSort::Default,
        }
    }

    pub(crate) fn set_rows(&mut self, rows: Vec<FeeRow>) {
        let mut sorted = rows;
        sorted.sort_by(|a, b| {
            a.chain_id
                .cmp(&b.chain_id)
                .then_with(|| a.railgun_address.cmp(&b.railgun_address))
                .then_with(|| a.token_address.cmp(&b.token_address))
        });
        self.all_rows = Arc::from(sorted);
        self.rebuild_visible();
    }

    pub(crate) fn set_chain_filter(&mut self, filter: FeesFilter<u64>) {
        let changed = self.chain_filter != filter;
        self.chain_filter = filter;
        // Chain changes reset to the chain's native broadcaster fee token.
        // The broadcaster query is a substring — it self-corrects when rows
        // stop matching, no reset needed.
        self.token_filter = if changed {
            self.default_token_filter(filter)
        } else {
            cascade_reset_token(filter, self.token_filter)
        };
        self.rebuild_visible();
    }

    pub(crate) fn set_broadcaster_query(&mut self, query: Arc<str>) {
        self.broadcaster_query = query;
        self.rebuild_visible();
    }

    pub(crate) fn set_token_filter(&mut self, filter: FeesFilter<(u64, Address)>) {
        self.token_filter = filter;
        self.rebuild_visible();
    }

    /// Advance fee sort through Default → Descending → Ascending → Default,
    /// matching the cycle `gpui_component::table::DataTable` uses for its built-in
    /// sort icon.
    pub(crate) fn toggle_fee_sort(&mut self) {
        self.fee_sort = match self.fee_sort {
            ColumnSort::Default => ColumnSort::Descending,
            ColumnSort::Descending => ColumnSort::Ascending,
            ColumnSort::Ascending => ColumnSort::Default,
        };
        self.bonus_sort = ColumnSort::Default;
        self.rebuild_visible();
    }

    /// Advance bonus sort through Default → Descending → Ascending → Default.
    pub(crate) fn toggle_bonus_sort(&mut self) {
        self.bonus_sort = match self.bonus_sort {
            ColumnSort::Default => ColumnSort::Descending,
            ColumnSort::Descending => ColumnSort::Ascending,
            ColumnSort::Ascending => ColumnSort::Default,
        };
        self.fee_sort = ColumnSort::Default;
        self.rebuild_visible();
    }

    pub(crate) fn refresh_anchor_values(&mut self) {
        self.rebuild_visible();
    }

    pub(crate) fn set_preference_hooks(&mut self, hooks: Option<BroadcasterPreferenceHooks>) {
        let enabled = hooks.is_some();
        self.preference_hooks = hooks;
        self.columns = fee_columns(enabled);
    }

    fn rebuild_visible(&mut self) {
        let chain_filter = self.chain_filter;
        let token_filter = self.token_filter;
        let query = self.broadcaster_query.clone();

        let mut rows: Vec<FeeRow> = self
            .all_rows
            .iter()
            .filter(|row| matches_chain(row, chain_filter))
            .filter(|row| matches_token(row, token_filter))
            .filter(|row| matches_broadcaster(row, &query))
            .cloned()
            .collect();
        // Sort by raw fee amount when requested. Raw comparison is stable
        // within a single (chain, token) group; cross-token ordering is
        // meaningful only in wei — callers comparing human-scale magnitudes
        // across tokens should filter by token first.
        match self.bonus_sort {
            ColumnSort::Default => match self.fee_sort {
                ColumnSort::Default => {}
                ColumnSort::Ascending => rows.sort_by_key(|row| row.fee),
                ColumnSort::Descending => rows.sort_by_key(|row| std::cmp::Reverse(row.fee)),
            },
            sort => {
                rows.sort_by(|a, b| compare_fee_bonus_rows(a, b, sort, &self.fee_anchor_lookup));
            }
        }
        self.rows = Arc::from(rows);
    }

    /// Unique token options across `all_rows`, scoped by the current chain
    /// filter. Sorted by (`chain_id`, symbol-or-address) for stable menu order.
    fn token_options(&self) -> Vec<(u64, Address)> {
        let mut seen: Vec<(u64, Address)> = Vec::new();
        for row in self.all_rows.iter() {
            if !matches_chain(row, self.chain_filter) {
                continue;
            }
            let key = (row.chain_id, row.token_address);
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        if let FeesFilter::One(chain_id) = self.chain_filter
            && let Some(token) = self.default_fee_token(chain_id)
        {
            let key = (chain_id, token);
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        if let FeesFilter::One(key) = self.token_filter
            && matches_chain_id(key.0, self.chain_filter)
            && !seen.contains(&key)
        {
            seen.push(key);
        }
        seen.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| token_label(a.0, &a.1).cmp(&token_label(b.0, &b.1)))
        });
        seen
    }

    fn default_fee_token(&self, chain_id: u64) -> Option<Address> {
        self.default_fee_tokens
            .iter()
            .find_map(|(default_chain_id, token)| (*default_chain_id == chain_id).then_some(*token))
    }

    fn default_token_filter(&self, filter: FeesFilter<u64>) -> FeesFilter<(u64, Address)> {
        default_token_filter_for_chain(filter, self.default_fee_tokens.as_slice())
    }

    fn token_filter_items(&self) -> Vec<FeesTokenFilterItem> {
        let show_chain = matches!(self.chain_filter, FeesFilter::All);
        std::iter::once(FeesTokenFilterItem::all(show_chain))
            .chain(
                self.token_options()
                    .into_iter()
                    .map(move |(chain_id, address)| {
                        FeesTokenFilterItem::token(chain_id, address, show_chain)
                    }),
            )
            .collect()
    }

    pub(crate) fn sync_filter_selects(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) {
        self.chain_select.update(cx, |select, cx| {
            if select.selected_value().copied() != Some(self.chain_filter) {
                select.set_selected_value(&self.chain_filter, window, cx);
            }
        });

        let token_items = self.token_filter_items();
        let token_items_changed = self.synced_token_filter_items != token_items;
        if token_items_changed {
            self.synced_token_filter_items.clone_from(&token_items);
        }
        self.token_select.update(cx, |select, cx| {
            let selected_value = select.selected_value().copied();
            if token_items_changed {
                select.set_items(SearchableVec::new(token_items), window, cx);
            }
            if token_items_changed || selected_value != Some(self.token_filter) {
                select.set_selected_value(&self.token_filter, window, cx);
            }
        });
    }
}
