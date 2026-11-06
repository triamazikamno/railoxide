//! A search field modelled on Discord's search: free text plus filter chips, with suggestions
//! under the field drawn from a feature's rows. A feature describes its filters, chips and
//! values through [`SearchSource`]. Recent searches stay in memory for the session, since UI
//! state on disk is plaintext and they can hold addresses and hashes.

use std::fmt::Debug;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, ElementId, Entity, EventEmitter,
    Focusable as _, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, Pixels, Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _,
    Window, canvas, deferred, div, img, prelude::FluentBuilder as _, px, relative, rems, rgb,
};
use gpui_component::{
    Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Backspace, Enter, Escape, InputEvent, InputState, MoveDown, MoveUp},
    scroll::ScrollableElement as _,
    separator::Separator,
};
use gpui_kit::base::{Align, Placement, Positioner};
use ui::controls::{app_input, app_muted_text, app_text};
use ui::theme::{self, APP_TEXT_LINE_HEIGHT};

use crate::assets::WalletIconSource;
use crate::root::new_text_input;
use crate::root::ui_helpers::token_icon_slot;

const RECENT: &str = "Recent";
const RECENT_LIMIT: usize = 5;
/// Values listed for a pending filter.
const VALUES_LIMIT: usize = 8;
/// Matches listed per filter for free text.
const MATCHES_LIMIT: usize = 5;
const SUGGESTIONS_MAX_HEIGHT: Pixels = px(320.0);

/// How a filter's chips combine: a row passes an `Any` filter when it matches one of its
/// chips, and an `All` filter when it matches every one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum ChipCombine {
    Any,
    All,
}

/// A feature's filters, chips and values, which the field shows and suggests.
pub(in crate::root) trait SearchSource: 'static {
    /// A filter, which chips of one kind belong to.
    type Facet: Copy + Eq + Debug + 'static;
    type Chip: Clone + Eq + Debug + 'static;
    /// Starts the field's element ids, such as `<prefix>-suggestions`.
    const ID_PREFIX: &'static str;
    const PLACEHOLDER: &'static str;
    /// Every filter, in the order the empty field lists them.
    const FACETS: &'static [Self::Facet];

    /// Typed before a value, such as `token:`.
    fn keyword(facet: Self::Facet) -> &'static str;
    fn title(facet: Self::Facet) -> &'static str;
    /// Example values, shown beside the filter.
    fn hint(facet: Self::Facet) -> &'static str;
    fn combine(facet: Self::Facet) -> ChipCombine;
    fn chip_facet(chip: &Self::Chip) -> Self::Facet;
    /// The value a chip shows, and its icon.
    fn chip_value(&self, chip: &Self::Chip) -> (String, Option<WalletIconSource>);
    /// Up to `limit` values of `facet` that hold `query`, lowercased.
    fn facet_values(
        &self,
        facet: Self::Facet,
        query: &str,
        limit: usize,
    ) -> Vec<Suggestion<Self::Facet, Self::Chip>>;
    /// The filters whose values free text `query`, lowercased, lists, in order.
    fn text_facets(&self, query: &str) -> Vec<Self::Facet>;
}

/// The search field's free text and chips. An empty one matches every row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SearchFilter<C> {
    /// Trimmed and lowercased.
    query: String,
    pub(in crate::root) chips: Vec<C>,
}

impl<C> Default for SearchFilter<C> {
    fn default() -> Self {
        Self {
            query: String::new(),
            chips: Vec::new(),
        }
    }
}

impl<C> SearchFilter<C> {
    /// Trimmed and lowercased.
    pub(in crate::root) fn query(&self) -> &str {
        &self.query
    }

    pub(in crate::root) fn set_query(&mut self, query: &str) {
        self.query = query.trim().to_lowercase();
    }

    /// Whether a row passes every filter that has chips, given whether it `matches` a chip:
    /// one of an `Any` filter's chips, every one of an `All` filter's.
    pub(in crate::root) fn chips_match<S: SearchSource<Chip = C>>(
        &self,
        matches: impl Fn(&C) -> bool,
    ) -> bool {
        S::FACETS.iter().all(|&facet| {
            let mut chips = self
                .chips
                .iter()
                .filter(|chip| S::chip_facet(chip) == facet);
            match S::combine(facet) {
                ChipCombine::Any => {
                    let mut chips = chips.peekable();
                    chips.peek().is_none() || chips.any(&matches)
                }
                ChipCombine::All => chips.all(&matches),
            }
        })
    }
}

/// A recent search: the chips and free text in effect when it was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct RecentSearch<C> {
    pub(in crate::root) chips: Vec<C>,
    pub(in crate::root) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) enum SuggestionAction<F, C> {
    AddChip(C),
    /// Picks a filter, whose values the suggestions then list.
    Pending(F),
    /// "Search for …": runs the field's text.
    SearchText,
    /// Replaces the chips and text with recent search `n`.
    Recent(usize),
}

#[derive(Clone, Debug)]
pub(in crate::root) struct Suggestion<F, C> {
    pub(in crate::root) title: String,
    /// A keyword, shown bold, and the value it filters by.
    pub(in crate::root) secondary: Option<(&'static str, String)>,
    /// Shown at the end, such as how many operations move a token.
    pub(in crate::root) note: Option<String>,
    pub(in crate::root) icon: Option<WalletIconSource>,
    pub(in crate::root) action: SuggestionAction<F, C>,
}

#[derive(Clone, Debug)]
pub(in crate::root) struct SuggestionGroup<F, C> {
    /// `None` for the leading "Search for …".
    pub(in crate::root) title: Option<&'static str>,
    pub(in crate::root) items: Vec<Suggestion<F, C>>,
}

/// The suggestions under the field, in groups, none of them empty.
///
/// - With a pending filter: its values, narrowed by `text`.
/// - With no text: the filters, then the recent searches.
/// - With text: "Search for" it, then the filters and the values of each filter that match it.
pub(in crate::root) fn suggestions<S: SearchSource>(
    source: &S,
    text: &str,
    pending: Option<S::Facet>,
    recent: &[RecentSearch<S::Chip>],
) -> Vec<SuggestionGroup<S::Facet, S::Chip>> {
    let text = text.trim();
    let query = text.to_lowercase();
    let group = |title, items| SuggestionGroup { title, items };
    let mut groups = Vec::new();
    if let Some(facet) = pending {
        groups.push(group(
            Some(S::title(facet)),
            source.facet_values(facet, &query, VALUES_LIMIT),
        ));
    } else if query.is_empty() {
        groups.push(group(
            Some("Filters"),
            S::FACETS
                .iter()
                .copied()
                .map(filter_suggestion::<S>)
                .collect(),
        ));
        groups.push(group(
            Some(RECENT),
            (0..recent.len())
                .map(|index| suggestion(String::new(), None, SuggestionAction::Recent(index)))
                .collect(),
        ));
    } else {
        groups.push(group(
            None,
            vec![suggestion(
                format!("Search for {text}"),
                None,
                SuggestionAction::SearchText,
            )],
        ));
        groups.push(group(
            Some("Filters"),
            S::FACETS
                .iter()
                .copied()
                .filter(|&facet| {
                    S::title(facet).to_lowercase().starts_with(&query)
                        || S::keyword(facet).starts_with(&query)
                })
                .map(filter_suggestion::<S>)
                .collect(),
        ));
        for facet in source.text_facets(&query) {
            groups.push(group(
                Some(S::title(facet)),
                source.facet_values(facet, &query, MATCHES_LIMIT),
            ));
        }
    }
    groups.retain(|group| !group.items.is_empty());
    groups
}

/// The filter whose keyword, in any case, starts `text`, and the text after it.
pub(in crate::root) fn split_keyword<S: SearchSource>(text: &str) -> Option<(S::Facet, &str)> {
    let text = text.trim_start();
    S::FACETS.iter().find_map(|&facet| {
        let keyword = S::keyword(facet);
        text.get(..keyword.len())
            .filter(|head| head.eq_ignore_ascii_case(keyword))
            .map(|_| (facet, &text[keyword.len()..]))
    })
}

pub(in crate::root) const fn suggestion<F, C>(
    title: String,
    secondary: Option<(&'static str, String)>,
    action: SuggestionAction<F, C>,
) -> Suggestion<F, C> {
    Suggestion {
        title,
        secondary,
        note: None,
        icon: None,
        action,
    }
}

fn filter_suggestion<S: SearchSource>(facet: S::Facet) -> Suggestion<S::Facet, S::Chip> {
    suggestion(
        S::title(facet).to_owned(),
        Some((S::keyword(facet), S::hint(facet).to_owned())),
        SuggestionAction::Pending(facet),
    )
}

/// The list child that shows suggestion `index`, counting group dividers and headers.
fn suggestion_child_index<F, C>(groups: &[SuggestionGroup<F, C>], index: usize) -> usize {
    let mut child = 0;
    let mut remaining = index;
    for (group_index, group) in groups.iter().enumerate() {
        child += usize::from(group_index > 0) + usize::from(group.title.is_some());
        if remaining < group.items.len() {
            return child + remaining;
        }
        child += group.items.len();
        remaining -= group.items.len();
    }
    child
}

/// A chip: its keyword, muted, then its value with a token's icon.
fn render_chip(
    keyword: &'static str,
    value: Option<(String, Option<WalletIconSource>)>,
) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap_1()
        .px(px(6.0))
        .py(px(1.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_ELEVATED))
        .child(
            app_text(keyword)
                .flex_none()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD),
        )
        .when_some(value, |this, (value, icon)| {
            this.when_some(icon, |this, icon| {
                this.child(img(icon).size_3p5().flex_none().rounded_full())
            })
            .child(
                app_text(value)
                    .flex_none()
                    .text_xs()
                    .font_weight(FontWeight::NORMAL),
            )
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum SearchFieldEvent {
    /// The applied query or chips changed.
    FilterChanged,
}

/// The search field: its input, filter, pending filter, suggestions and recent searches.
pub(in crate::root) struct SearchField<S: SearchSource> {
    source: S,
    /// Sets the input's text from updates that don't hold the window.
    window: AnyWindowHandle,
    input: Entity<InputState>,
    filter: SearchFilter<S::Chip>,
    /// The filter last announced with [`SearchFieldEvent::FilterChanged`].
    applied: SearchFilter<S::Chip>,
    /// A filter picked or typed with no value yet, whose values the suggestions list.
    pending: Option<S::Facet>,
    /// The field's text. With a pending filter it narrows the filter's values; else it is the
    /// filter's query.
    text: String,
    open: bool,
    /// Whether the press under way began in the already focused field, which toggles the
    /// suggestions.
    press_toggles: bool,
    /// The highlighted suggestion, counted across groups.
    highlighted: usize,
    groups: Vec<SuggestionGroup<S::Facet, S::Chip>>,
    /// Newest first, in memory only.
    recent: Vec<RecentSearch<S::Chip>>,
    /// The field's window bounds, measured each frame, which anchor the suggestions.
    bounds: Entity<Option<Bounds<Pixels>>>,
    scroll: ScrollHandle,
}

impl<S: SearchSource> EventEmitter<SearchFieldEvent> for SearchField<S> {}

impl<S: SearchSource> SearchField<S> {
    pub(in crate::root) fn new(source: S, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let input = new_text_input(window, cx, S::PLACEHOLDER);
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                let text = input.read(cx).value().to_string();
                this.set_query(&text, cx);
            }
            InputEvent::PressEnter { .. } => this.search_text(cx),
            InputEvent::Focus => this.show_suggestions(true, cx),
            InputEvent::Blur => this.show_suggestions(false, cx),
        })
        .detach();
        let groups = suggestions(&source, "", None, &[]);
        Self {
            source,
            window: window.window_handle(),
            input,
            filter: SearchFilter::default(),
            applied: SearchFilter::default(),
            pending: None,
            text: String::new(),
            open: false,
            press_toggles: false,
            highlighted: 0,
            groups,
            recent: Vec::new(),
            bounds: cx.new(|_| None),
            scroll: ScrollHandle::new(),
        }
    }

    pub(in crate::root) const fn filter(&self) -> &SearchFilter<S::Chip> {
        &self.filter
    }

    /// Follows the feature's rows: recomputes the suggestions from `source`.
    pub(in crate::root) fn set_source(&mut self, source: S, cx: &mut Context<'_, Self>) {
        self.source = source;
        self.apply(cx);
    }

    /// Keeps the chips `keep` accepts.
    pub(in crate::root) fn retain_chips(
        &mut self,
        keep: impl FnMut(&S::Chip) -> bool,
        cx: &mut Context<'_, Self>,
    ) {
        let count = self.filter.chips.len();
        self.filter.chips.retain(keep);
        if self.filter.chips.len() != count {
            self.apply(cx);
        }
    }

    /// Forgets the session's search: its text, chips, pending filter and recent searches.
    pub(in crate::root) fn end_session(&mut self, cx: &mut Context<'_, Self>) {
        self.recent.clear();
        self.highlighted = 0;
        self.filter = SearchFilter::default();
        self.pending = None;
        self.text.clear();
        self.open = false;
        self.set_field(String::new(), cx);
        self.apply(cx);
    }

    /// Sets the field's text, which needs the window an entity update doesn't hold. Setting
    /// it emits no change.
    fn set_field(&self, text: String, cx: &mut App) {
        let input = self.input.clone();
        let window = self.window;
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                input.update(cx, |input, cx| input.set_value(text, window, cx));
            });
        });
    }

    /// Recomputes the query and suggestions, and announces a changed query or chips.
    fn apply(&mut self, cx: &mut Context<'_, Self>) {
        let query = if self.pending.is_some() {
            ""
        } else {
            self.text.as_str()
        };
        self.filter.set_query(query);
        self.groups = suggestions(&self.source, &self.text, self.pending, &self.recent);
        if self.suggestion(self.highlighted).is_none() {
            self.highlighted = 0;
        }
        if self.filter != self.applied {
            self.applied = self.filter.clone();
            cx.emit(SearchFieldEvent::FilterChanged);
        }
        cx.notify();
    }

    pub(in crate::root) const fn suggestions_shown(&self) -> bool {
        self.open && !self.groups.is_empty()
    }

    fn suggestion(&self, index: usize) -> Option<&Suggestion<S::Facet, S::Chip>> {
        self.groups.iter().flat_map(|group| &group.items).nth(index)
    }

    /// Records the chips and text in effect as the newest recent search.
    fn remember(&mut self) {
        let search = RecentSearch {
            chips: self.filter.chips.clone(),
            text: self.text.trim().to_owned(),
        };
        if search.chips.is_empty() && search.text.is_empty() {
            return;
        }
        self.recent.retain(|recent| *recent != search);
        self.recent.insert(0, search);
        self.recent.truncate(RECENT_LIMIT);
    }

    /// Follows the field's text. A filter keyword at its start becomes the pending filter.
    pub(in crate::root) fn set_query(&mut self, text: &str, cx: &mut Context<'_, Self>) {
        let typed = self
            .pending
            .is_none()
            .then(|| split_keyword::<S>(text))
            .flatten();
        if let Some((facet, rest)) = typed {
            self.pending = Some(facet);
            rest.trim_start().clone_into(&mut self.text);
        } else {
            text.clone_into(&mut self.text);
        }
        self.open = true;
        self.highlighted = 0;
        if typed.is_some() {
            self.set_field(self.text.clone(), cx);
        }
        self.apply(cx);
    }

    fn show_suggestions(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if self.open != open {
            self.open = open;
            self.highlighted = 0;
            cx.notify();
        }
    }

    /// Runs the field's free text, as "Search for …" or Enter: records it as a recent search
    /// and closes the suggestions.
    pub(in crate::root) fn search_text(&mut self, cx: &mut Context<'_, Self>) {
        if self.pending.is_none() && !self.text.trim().is_empty() {
            self.remember();
        }
        self.open = false;
        self.apply(cx);
    }

    fn add_chip(&mut self, chip: S::Chip, cx: &mut Context<'_, Self>) {
        if !self.filter.chips.contains(&chip) {
            self.filter.chips.push(chip);
        }
        self.pending = None;
        self.text.clear();
        self.open = false;
        self.remember();
        self.set_field(String::new(), cx);
        self.apply(cx);
    }

    fn remove_chip(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        if index < self.filter.chips.len() {
            self.filter.chips.remove(index);
            self.apply(cx);
        }
    }

    /// The field's clear button: drops the chips, the pending filter and the text.
    pub(in crate::root) fn clear(&mut self, cx: &mut Context<'_, Self>) {
        self.filter.chips.clear();
        self.pending = None;
        self.text.clear();
        self.highlighted = 0;
        self.open = true;
        self.set_field(String::new(), cx);
        self.apply(cx);
    }

    fn apply_suggestion(
        &mut self,
        action: SuggestionAction<S::Facet, S::Chip>,
        cx: &mut Context<'_, Self>,
    ) {
        match action {
            SuggestionAction::AddChip(chip) => self.add_chip(chip, cx),
            SuggestionAction::Pending(facet) => {
                self.pending = Some(facet);
                self.text.clear();
                self.open = true;
                self.highlighted = 0;
                self.set_field(String::new(), cx);
                self.apply(cx);
            }
            SuggestionAction::SearchText => self.search_text(cx),
            SuggestionAction::Recent(index) => {
                let Some(recent) = self.recent.get(index).cloned() else {
                    return;
                };
                self.filter.chips = recent.chips;
                self.pending = None;
                self.text.clone_from(&recent.text);
                self.open = false;
                self.set_field(recent.text, cx);
                self.apply(cx);
            }
        }
    }

    fn clear_recent(&mut self, cx: &mut Context<'_, Self>) {
        self.recent.clear();
        self.apply(cx);
    }

    fn highlight(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        if self.highlighted != index {
            self.highlighted = index;
            cx.notify();
        }
    }

    /// Up or Down while suggestions are shown; `false` when none are.
    fn move_highlight(&mut self, down: bool, cx: &mut Context<'_, Self>) -> bool {
        let count = self
            .groups
            .iter()
            .map(|group| group.items.len())
            .sum::<usize>();
        if !self.suggestions_shown() || count == 0 {
            return false;
        }
        self.highlighted = if down {
            (self.highlighted + 1) % count
        } else {
            (self.highlighted + count - 1) % count
        };
        self.scroll
            .scroll_to_item(suggestion_child_index(&self.groups, self.highlighted));
        cx.notify();
        true
    }

    /// Enter while suggestions are shown applies the highlighted one; `false` when none are.
    fn apply_highlighted(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let Some(action) = self
            .suggestions_shown()
            .then(|| self.suggestion(self.highlighted))
            .flatten()
            .map(|suggestion| suggestion.action.clone())
        else {
            return false;
        };
        self.apply_suggestion(action, cx);
        true
    }

    /// Escape while suggestions are shown closes them; `false` when none are.
    pub(in crate::root) fn dismiss_suggestions(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if !self.suggestions_shown() {
            return false;
        }
        self.show_suggestions(false, cx);
        true
    }

    /// Backspace in the empty field clears the pending filter, else removes the last chip;
    /// `false` when the field has text or there is nothing to remove.
    fn backspace(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if !self.text.is_empty() {
            return false;
        }
        if self.pending.is_some() {
            self.pending = None;
        } else if self.filter.chips.pop().is_none() {
            return false;
        }
        self.apply(cx);
        true
    }

    fn element_id(suffix: &str) -> String {
        format!("{}-{suffix}", S::ID_PREFIX)
    }

    /// The suggestion list, as wide as the field: groups under section headers, with dividers
    /// between them.
    fn render_suggestions(&self, view: &Entity<Self>, width: Pixels) -> gpui::Div {
        let mut list = div()
            .id(Self::element_id("suggestions"))
            .max_h(SUGGESTIONS_MAX_HEIGHT)
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col();
        let mut index = 0;
        for (group_index, group) in self.groups.iter().enumerate() {
            if group_index > 0 {
                list = list.child(
                    Separator::horizontal()
                        .h(px(1.0))
                        .my(px(4.0))
                        .color(rgb(theme::BORDER_SUBTLE)),
                );
            }
            if let Some(title) = group.title {
                let clear_view = view.clone();
                list = list.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(4.0))
                        .child(
                            app_text(title.to_uppercase())
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                        .when(title == RECENT, |this| {
                            this.child(
                                div()
                                    // Keeps the press from toggling the suggestions.
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(
                                        Button::new(Self::element_id("recent-clear"))
                                            .text()
                                            .xsmall()
                                            .compact()
                                            .tab_stop(false)
                                            .accessibility_label("Clear recent searches")
                                            // The extra-small button sets the 12px size.
                                            .child(
                                                div()
                                                    .line_height(relative(APP_TEXT_LINE_HEIGHT))
                                                    .text_color(rgb(theme::PRIMARY))
                                                    .child("Clear"),
                                            )
                                            .on_click(move |_, _, cx| {
                                                clear_view.update(cx, Self::clear_recent);
                                            }),
                                    ),
                            )
                        }),
                );
            }
            for item in &group.items {
                list = list.child(self.render_suggestion(view, item, index));
                index += 1;
            }
        }
        div()
            .w(width)
            .max_w(rems(28.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::POPOVER_BG))
            .overflow_hidden()
            .occlude()
            .debug_selector(|| "wallet-search-suggestions".into())
            .child(list)
            .vertical_scrollbar(&self.scroll)
    }

    /// One suggestion: a title over its keyword and value, or a recent search's chips.
    fn render_suggestion(
        &self,
        view: &Entity<Self>,
        item: &Suggestion<S::Facet, S::Chip>,
        index: usize,
    ) -> impl IntoElement {
        let content = match &item.action {
            SuggestionAction::Recent(recent) => {
                let recent = self.recent.get(*recent);
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .children(
                        recent
                            .into_iter()
                            .flat_map(|recent| &recent.chips)
                            .map(|chip| {
                                render_chip(
                                    S::keyword(S::chip_facet(chip)),
                                    Some(self.source.chip_value(chip)),
                                )
                            }),
                    )
                    .children(
                        recent
                            .filter(|recent| !recent.text.is_empty())
                            .map(|recent| app_text(recent.text.clone()).truncate()),
                    )
            }
            _ => div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .child(app_text(item.title.clone()).truncate())
                .when_some(item.secondary.clone(), |this, (keyword, value)| {
                    this.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w(px(0.0))
                            .child(
                                app_text(keyword)
                                    .flex_none()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD),
                            )
                            .child(app_muted_text(value).min_w(px(0.0)).text_xs().truncate()),
                    )
                }),
        };
        let hover_view = view.clone();
        let click_view = view.clone();
        let action = item.action.clone();
        div()
            .id(ElementId::named_usize(
                Self::element_id("suggestion"),
                index,
            ))
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px(px(8.0))
            .py(px(4.0))
            .rounded_sm()
            .cursor_pointer()
            .when(index == self.highlighted, |this| {
                this.bg(rgb(theme::SURFACE_HOVER))
            })
            .on_hover(move |hovered, _, cx| {
                if *hovered {
                    hover_view.update(cx, |field, cx| field.highlight(index, cx));
                }
            })
            // A press, not a click, so the field keeps its focus and the suggestions stay open.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                cx.stop_propagation();
                click_view.update(cx, |field, cx| field.apply_suggestion(action.clone(), cx));
            })
            .when_some(item.icon.clone(), |this, icon| {
                this.child(token_icon_slot(Some(icon)))
            })
            .child(content)
            .when_some(item.note.clone(), |this, note| {
                this.child(app_muted_text(note).flex_none().text_xs())
            })
    }
}

impl<S: SearchSource> SearchField<S> {
    #[allow(dead_code)]
    pub(in crate::root) const fn source(&self) -> &S {
        &self.source
    }

    /// Changes the chips with `edit`.
    pub(in crate::root) fn edit_chips(
        &mut self,
        edit: impl FnOnce(&mut Vec<S::Chip>),
        cx: &mut Context<'_, Self>,
    ) {
        edit(&mut self.filter.chips);
        self.apply(cx);
    }

    // Only tests read it.
    #[allow(dead_code)]
    pub(in crate::root) fn recent(&self) -> &[RecentSearch<S::Chip>] {
        &self.recent
    }

    pub(in crate::root) const fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    pub(in crate::root) fn text(&self) -> &str {
        &self.text
    }

    pub(in crate::root) fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }
}

/// The search field, with its chips in the input's prefix, and the suggestions open below it, or
/// above it when there is no room below, while it is focused. Up, Down, Enter, Escape and Backspace are taken in the capture
/// phase, before the input's own bindings, only when they act on the suggestions or chips.
impl<S: SearchSource> Render for SearchField<S> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let view = cx.entity();
        let chips = div()
            .flex()
            .items_center()
            .gap_1()
            .children(self.filter.chips.iter().enumerate().map(|(index, chip)| {
                let view = view.clone();
                render_chip(
                    S::keyword(S::chip_facet(chip)),
                    Some(self.source.chip_value(chip)),
                )
                .child(
                    div()
                        .id(ElementId::named_usize(
                            Self::element_id("chip-remove"),
                            index,
                        ))
                        .flex_none()
                        .cursor_pointer()
                        .child(
                            Icon::new(IconName::Close)
                                .xsmall()
                                .text_color(rgb(theme::TEXT_MUTED)),
                        )
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            cx.stop_propagation();
                            view.update(cx, |field, cx| field.remove_chip(index, cx));
                        }),
                )
            }))
            .children(
                self.pending
                    .map(|facet| render_chip(S::keyword(facet), None)),
            );
        let has_chips = !self.filter.chips.is_empty() || self.pending.is_some();
        let clear_view = view.clone();
        let clear = div()
            // Keeps the press from toggling the suggestions.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new(Self::element_id("search-clear"))
                    .icon(Icon::new(IconName::Close))
                    .ghost()
                    .xsmall()
                    .tab_stop(false)
                    .on_click(move |_, _, cx| {
                        clear_view.update(cx, Self::clear);
                    }),
            );
        let field = app_input(&self.input)
            .when(has_chips, |this| this.prefix(chips))
            .when(has_chips || !self.text.is_empty(), |this| {
                this.suffix(clear)
            });
        let measured = self.bounds.clone();
        let bounds = *self.bounds.read(cx);
        // Takes a key from the input only when `handle` acted on it.
        let key = |handle: fn(&mut Self, &mut Context<'_, Self>) -> bool| {
            let view = view.clone();
            move |cx: &mut App| {
                if view.update(cx, handle) {
                    cx.stop_propagation();
                }
            }
        };
        let (up, down, enter, escape, backspace) = (
            key(|field, cx| field.move_highlight(false, cx)),
            key(|field, cx| field.move_highlight(true, cx)),
            key(Self::apply_highlighted),
            key(Self::dismiss_suggestions),
            key(Self::backspace),
        );
        div()
            .relative()
            .flex_1()
            .min_w(px(0.0))
            .capture_action(move |_: &MoveUp, _, cx| up(cx))
            .capture_action(move |_: &MoveDown, _, cx| down(cx))
            .capture_action(move |_: &Enter, _, cx| enter(cx))
            .capture_action(move |_: &Escape, _, cx| escape(cx))
            .capture_action(move |_: &Backspace, _, cx| backspace(cx))
            .capture_any_mouse_down({
                let view = view.clone();
                let input = self.input.clone();
                move |_, window, cx| {
                    let focused = input.read(cx).focus_handle(cx).is_focused(window);
                    view.update(cx, |field, _| field.press_toggles = focused);
                }
            })
            .on_mouse_down(MouseButton::Left, {
                let view = view.clone();
                move |_, _, cx| {
                    view.update(cx, |field, cx| {
                        if std::mem::take(&mut field.press_toggles) {
                            let open = !field.open;
                            field.show_suggestions(open, cx);
                        }
                    });
                }
            })
            .child(field)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        if *measured.read(cx) != Some(bounds) {
                            measured.update(cx, |measured, _| *measured = Some(bounds));
                            window.request_animation_frame();
                        }
                    },
                    |_, (), _, _| {},
                )
                // Measures the field's frame, not the static position after it.
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .when_some(
                bounds.filter(|_| self.suggestions_shown()),
                |this, bounds| {
                    this.child(
                        deferred(
                            Positioner::side(bounds)
                                .placement(Placement::Bottom)
                                .align(Align::Start)
                                .offset(px(4.0))
                                .margin(px(8.0))
                                .child(self.render_suggestions(&view, bounds.size.width)),
                        )
                        .with_priority(gpui_kit::base::POPUP_PRIORITY),
                    )
                },
            )
    }
}
