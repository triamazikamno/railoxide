//! Searchable network choices shared by desktop and extension.
use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Styled, Task, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::{
    Icon, IconName, IndexPath, Sizable as _,
    button::{Button, ButtonVariants as _},
    searchable_list::SearchableGroup,
    select::{SearchableVec, Select, SelectDelegate, SelectItem, SelectState},
    separator::Separator,
};

/// Open a chain's settings without changing the selected network.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct EditChain {
    pub chain_id: u64,
}

#[derive(Clone)]
pub struct ChainSelectItem {
    pub chain_id: u64,
    pub label: SharedString,
}

impl SelectItem for ChainSelectItem {
    type Value = u64;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn display_title(&self) -> Option<AnyElement> {
        Some(
            crate::wallet_identity::chain_label_row(
                self.label.clone(),
                railgun_ui::chain_icon_asset_path(self.chain_id),
            )
            .into_any_element(),
        )
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        crate::wallet_identity::chain_label_row(
            self.label.clone(),
            railgun_ui::chain_icon_asset_path(self.chain_id),
        )
        .debug_selector(|| format!("chain-option-{}", self.chain_id))
    }

    fn value(&self) -> &u64 {
        &self.chain_id
    }

    fn matches(&self, query: &str) -> bool {
        self.label.to_lowercase().contains(&query.to_lowercase())
            || self.chain_id.to_string().contains(query)
    }
}

pub struct ChainSelectItems(SearchableVec<SearchableGroup<ChainSelectItem>>);

impl ChainSelectItems {
    #[must_use]
    pub fn new(mut items: Vec<ChainSelectItem>) -> Self {
        items.sort_by_key(|item| {
            (
                railgun_ui::built_in_chain_ids()
                    .position(|id| id == item.chain_id)
                    .unwrap_or(usize::MAX),
                item.chain_id,
            )
        });
        // The divider separates Railgun chains from public-only ones, preset or custom.
        let groups: (Vec<_>, Vec<_>) = items
            .into_iter()
            .partition(|item| railgun_ui::DEFAULT_CHAINS.contains(&item.chain_id));
        let groups: [Vec<_>; 2] = groups.into();
        let groups = groups
            .into_iter()
            .filter(|items| !items.is_empty())
            .map(|items| SearchableGroup::new("").items(items))
            .collect::<Vec<_>>();
        Self(SearchableVec::new(groups))
    }
}

impl SelectDelegate for ChainSelectItems {
    type Item = ChainSelectItem;

    fn sections_count(&self, cx: &App) -> usize {
        self.0.sections_count(cx)
    }

    fn items_count(&self, section: usize) -> usize {
        self.0.items_count(section)
    }

    fn item(&self, ix: IndexPath) -> Option<&Self::Item> {
        self.0.item(ix)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SelectItem<Value = V>,
        V: PartialEq,
    {
        self.0.position(value)
    }

    fn perform_search(&mut self, query: &str, window: &mut Window, cx: &mut App) -> Task<()> {
        self.0.perform_search(query, window, cx)
    }

    fn render_item(
        &self,
        _: IndexPath,
        item: &Self::Item,
        checked: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let chain_id = item.chain_id;
        if !window.is_action_available(&EditChain { chain_id }, cx) {
            return None;
        }
        let label = SharedString::from(format!("Edit {}", item.label));
        Some(
            div()
                .group("chain-select-row")
                .flex()
                .items_center()
                .gap_1()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(item.render(window, cx)),
                )
                .child(
                    div()
                        .invisible()
                        .group_hover("chain-select-row", Styled::visible)
                        .child(
                            Button::new(("edit-chain", chain_id))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Settings)
                                .tab_stop(false)
                                .accessibility_label(label.clone())
                                .tooltip(label)
                                .debug_selector(move || format!("edit-chain-{chain_id}"))
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    if let Some(focus) = window.focused(cx) {
                                        focus.dispatch_action(
                                            &gpui_kit::base::actions::Cancel,
                                            window,
                                            cx,
                                        );
                                    }
                                    // Let Select restore focus before the editor takes it.
                                    window.defer(cx, move |window, cx| {
                                        window
                                            .dispatch_action(Box::new(EditChain { chain_id }), cx);
                                    });
                                }),
                        ),
                )
                .child(
                    Icon::new(IconName::Check)
                        .xsmall()
                        .when(!checked, Styled::invisible),
                )
                .into_any_element(),
        )
    }

    fn render_section_header(
        &self,
        section: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        // The virtual list measures section zero and reuses its height for every header.
        // Reserve the same space above the first group, but only draw between groups.
        (self.sections_count(cx) > 1).then(|| {
            Separator::horizontal()
                .py_1()
                .when(section == 0, Styled::invisible)
                .into_any_element()
        })
    }
}

pub fn chain_select_state(
    items: Vec<ChainSelectItem>,
    selected: Option<u64>,
    window: &mut Window,
    cx: &mut Context<'_, SelectState<ChainSelectItems>>,
) -> SelectState<ChainSelectItems> {
    let items = ChainSelectItems::new(items);
    let selected = selected.and_then(|id| items.position(&id));
    SelectState::new(items, selected, window, cx).searchable(true)
}

#[must_use]
pub fn chain_select(state: &Entity<SelectState<ChainSelectItems>>) -> Select<ChainSelectItems> {
    Select::new(state)
        .accessibility_label("Network")
        .search_placeholder("Search networks")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Render, TestAppContext, VisualTestContext, div, px};

    const LARGE_CHAIN: u64 = 9_007_199_254_740_993;

    fn items() -> Vec<ChainSelectItem> {
        [
            (31337, "Anvil"),
            (42161, "Arbitrum"),
            (LARGE_CHAIN, "Custom"),
            (1, "Ethereum"),
        ]
        .into_iter()
        .map(|(chain_id, label)| ChainSelectItem {
            chain_id,
            label: label.into(),
        })
        .collect()
    }

    #[gpui::test]
    fn search_filters_groups_and_removes_unneeded_dividers(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            let mut choices = ChainSelectItems::new(items());
            assert_eq!(choices.sections_count(cx), 2);
            assert_eq!(choices.item(IndexPath::new(1)).unwrap().chain_id, 42161);
            assert_eq!(choices.position(&31337), Some(IndexPath::new(0).section(1)));
            assert!(choices.render_section_header(1, window, cx).is_some());
            choices.perform_search("aNvIl", window, cx).detach();
            assert_eq!(choices.sections_count(cx), 1);
            assert_eq!(choices.items_count(0), 1);
            assert_eq!(choices.position(&31337), Some(IndexPath::new(0)));
            assert!(choices.render_section_header(0, window, cx).is_none());
            choices
                .perform_search("no such network", window, cx)
                .detach();
            assert_eq!(choices.sections_count(cx), 0);
            choices.perform_search("", window, cx).detach();
            assert_eq!(choices.sections_count(cx), 2);
            assert_eq!(
                choices.position(&LARGE_CHAIN),
                Some(IndexPath::new(1).section(1))
            );
        });
    }

    struct SelectorProbe {
        select: Entity<SelectState<ChainSelectItems>>,
        editing_enabled: bool,
        edited_chain: Option<u64>,
        editor_focus: gpui::FocusHandle,
    }

    impl Render for SelectorProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            div()
                .track_focus(&self.editor_focus)
                .when(self.editing_enabled, |this| {
                    this.on_action(cx.listener(|this, action: &EditChain, window, cx| {
                        this.edited_chain = Some(action.chain_id);
                        this.editor_focus.focus(window, cx);
                        cx.notify();
                    }))
                })
                .p_4()
                .child(chain_select(&self.select).w(px(240.0)))
        }
    }

    #[gpui::test]
    fn keyboard_search_and_registry_refresh_preserve_exact_chain_identity(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut select = None;
        let handle = cx.add_window(|window, cx| {
            let state = cx.new(|cx| chain_select_state(items(), Some(31337), window, cx));
            select = Some(state.clone());
            let view = cx.new(|cx| SelectorProbe {
                select: state,
                editing_enabled: true,
                edited_chain: None,
                editor_focus: cx.focus_handle(),
            });
            gpui_component::Root::new(view, window, cx)
        });
        let select = select.unwrap();
        let cx = VisualTestContext::from_window(*handle, cx).into_mut();
        cx.update(|window, cx| {
            assert_eq!(select.read(cx).selected_value(), Some(&31337));
            select.update(cx, |state, cx| state.focus(window, cx));
        });
        cx.refresh().unwrap();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        for rem in [16.0, 24.0] {
            cx.update(|window, _| window.set_rem_size(px(rem)));
            cx.refresh().unwrap();
            let ethereum = cx.debug_bounds("chain-option-1").unwrap();
            let arbitrum = cx.debug_bounds("chain-option-42161").unwrap();
            let base = cx.debug_bounds("chain-option-31337").unwrap();
            let row_gap = arbitrum.top() - ethereum.bottom();
            let group_gap = base.top() - arbitrum.bottom();
            assert!(
                group_gap > row_gap,
                "the divider must occupy space between rows: group gap {group_gap:?}, row gap {row_gap:?}"
            );
        }
        cx.simulate_input(&LARGE_CHAIN.to_string());
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(select.read(cx).selected_value(), Some(&LARGE_CHAIN)));
        cx.update(|window, cx| {
            select.update(cx, |state, cx| {
                let mut updated = items();
                updated.retain(|chain| chain.chain_id != 31337);
                state.set_items(ChainSelectItems::new(updated), window, cx);
                state.set_selected_value(&LARGE_CHAIN, window, cx);
            });
            assert_eq!(select.read(cx).selected_value(), Some(&LARGE_CHAIN));
            assert_eq!(
                select.read(cx).selected_index(cx),
                Some(IndexPath::new(0).section(1))
            );
        });
        cx.simulate_keystrokes("enter");
        cx.simulate_input("eThEr");
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(select.read(cx).selected_value(), Some(&LARGE_CHAIN)));
    }

    #[gpui::test]
    fn editing_a_filtered_chain_dismisses_without_selecting_it(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut probe = None;
        let handle = cx.add_window(|window, cx| {
            let select = cx.new(|cx| chain_select_state(items(), Some(31337), window, cx));
            let view = cx.new(|cx| SelectorProbe {
                select,
                editing_enabled: true,
                edited_chain: None,
                editor_focus: cx.focus_handle(),
            });
            probe = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let probe = probe.unwrap();
        let cx = VisualTestContext::from_window(*handle, cx).into_mut();
        let select = cx.read(|cx| probe.read(cx).select.clone());
        cx.update(|window, cx| select.update(cx, |state, cx| state.focus(window, cx)));
        cx.refresh().unwrap();
        cx.simulate_keystrokes("enter");
        cx.simulate_input(&LARGE_CHAIN.to_string());
        cx.run_until_parked();
        let row = cx.debug_bounds("chain-option-9007199254740993").unwrap();
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
        cx.refresh().unwrap();
        let cog = cx.debug_bounds("edit-chain-9007199254740993").unwrap();
        cx.simulate_click(cog.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.read(|cx| {
            assert_eq!(probe.read(cx).edited_chain, Some(LARGE_CHAIN));
            assert_eq!(select.read(cx).selected_value(), Some(&31337));
        });
        assert!(cx.debug_bounds("chain-option-9007199254740993").is_none());
        cx.update(|window, cx| assert!(probe.read(cx).editor_focus.is_focused(window)));
        cx.update(|window, cx| select.update(cx, |state, cx| state.focus(window, cx)));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let row = cx.debug_bounds("chain-option-42161").unwrap();
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(select.read(cx).selected_value(), Some(&42161)));
        probe.update(cx, |view, cx| {
            view.editing_enabled = false;
            cx.notify();
        });
        cx.update(|window, cx| select.update(cx, |state, cx| state.focus(window, cx)));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let row = cx.debug_bounds("chain-option-42161").unwrap();
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("edit-chain-42161").is_none());
    }
}
