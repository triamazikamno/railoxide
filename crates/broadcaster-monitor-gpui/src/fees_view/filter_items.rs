#[allow(clippy::wildcard_imports)]
use super::*;

impl FeesChainFilterItem {
    pub(crate) const fn all() -> Self {
        Self {
            value: FeesFilter::All,
        }
    }

    pub(crate) const fn chain(chain_id: u64) -> Self {
        Self {
            value: FeesFilter::One(chain_id),
        }
    }
}

impl SelectItem for FeesChainFilterItem {
    type Value = FeesFilter<u64>;

    fn title(&self) -> SharedString {
        match self.value {
            FeesFilter::All => SharedString::from("All"),
            FeesFilter::One(chain_id) => SharedString::from(chain_label(chain_id)),
        }
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(self.render_title(px(16.0)).into_any_element())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.render_title(px(16.0))
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

impl FeesChainFilterItem {
    fn render_title(&self, icon_size: Pixels) -> gpui::AnyElement {
        match self.value {
            FeesFilter::All => div().child("All").into_any_element(),
            FeesFilter::One(chain_id) => icon_label_row(
                chain_id,
                SharedString::from(chain_label(chain_id)),
                icon_size,
            )
            .into_any_element(),
        }
    }
}

impl FeesTokenFilterItem {
    pub(crate) const fn all(show_chain: bool) -> Self {
        Self {
            value: FeesFilter::All,
            show_chain,
        }
    }

    pub(super) const fn token(chain_id: u64, address: Address, show_chain: bool) -> Self {
        Self {
            value: FeesFilter::One((chain_id, address)),
            show_chain,
        }
    }
}

impl SelectItem for FeesTokenFilterItem {
    type Value = FeesFilter<(u64, Address)>;

    fn title(&self) -> SharedString {
        match self.value {
            FeesFilter::All => SharedString::from("All"),
            FeesFilter::One((chain_id, address)) => {
                SharedString::from(token_menu_label(chain_id, &address, self.show_chain))
            }
        }
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(self.render_title(px(16.0)).into_any_element())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.render_title(px(16.0))
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.to_ascii_lowercase();
        match self.value {
            FeesFilter::All => "all".contains(&query),
            FeesFilter::One((chain_id, address)) => {
                chain_label(chain_id).to_ascii_lowercase().contains(&query)
                    || token_label(chain_id, &address)
                        .to_ascii_lowercase()
                        .contains(&query)
                    || address.to_string().to_ascii_lowercase().contains(&query)
            }
        }
    }
}

impl FeesTokenFilterItem {
    fn render_title(&self, icon_size: Pixels) -> gpui::AnyElement {
        match self.value {
            FeesFilter::All => div().child("All").into_any_element(),
            FeesFilter::One((chain_id, address)) => token_trigger_content(
                self.show_chain.then_some(chain_id),
                Some((chain_id, address)),
                SharedString::from(token_menu_label(chain_id, &address, self.show_chain)),
                icon_size,
            )
            .into_any_element(),
        }
    }
}
