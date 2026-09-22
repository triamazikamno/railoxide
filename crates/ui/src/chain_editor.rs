//! Chain management presentation. Hosts own authority, validation, persistence, and networking.
use crate::controls::{
    app_button, app_input, app_input_group, app_muted_text, app_strong_text, app_text,
};
use crate::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_LINE_HEIGHT};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, img, prelude::FluentBuilder as _, px,
    relative, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
    alert::Alert,
    button::{Button, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    collapsible::Collapsible,
    dialog::DialogButtonProps,
    input::{InputGroupAddon, InputGroupAddonAlignment, InputGroupButton, InputState},
    select::{Select, SelectEvent, SelectItem, SelectState},
    tag::Tag,
};
use railgun_ui::chain_editor::{
    ChainDraft, ChainEditorCommand, ChainEditorSnapshot, ChainField, ChainSummary, NativeUsdChoice,
    NativeUsdProbe, NativeUsdState, NativeUsdStatus,
};
use std::collections::BTreeMap;

/// Secondary text metrics shared by meta lines, section headers and field labels.
const META_TEXT_SIZE: Pixels = px(12.0);
const META_LINE_HEIGHT: Pixels = px(16.0);
const ROW_ICON_SIZE: Pixels = px(20.0);
const HEADER_ICON_SIZE: Pixels = px(24.0);
const CONFIRM_DIALOG_WIDTH: Pixels = px(380.0);

#[derive(Clone)]
#[non_exhaustive]
pub struct ChainEditorEvent {
    pub revision: String,
    pub command: ChainEditorCommand,
}

pub struct ChainEditor {
    snapshot: ChainEditorSnapshot,
    draft: Option<ChainDraft>,
    opened_draft: Option<ChainDraft>,
    pricing_select: Option<Entity<SelectState<Vec<PricingItem>>>>,
    pricing_status: BTreeMap<String, NativeUsdStatus>,
    pricing_preset: Option<Entity<InputState>>,
    /// The last Test reply and the draft source it read, so a stale result stays hidden.
    probe: Option<NativeUsdProbe>,
    probe_source: Option<String>,
    chain_id: Option<Entity<InputState>>,
    fields: BTreeMap<ChainField, FieldInput>,
    existing: bool,
    pending: bool,
    /// The host committed a different revision while this draft was open.
    stale: bool,
    advanced_open: bool,
    railgun_open: bool,
    edit_deployment: bool,
    error: Option<String>,
    focus: FocusHandle,
}

impl EventEmitter<ChainEditorEvent> for ChainEditor {}

/// The two collapsible groups whose open state the editor owns.
#[derive(Clone, Copy)]
enum Disclosure {
    Advanced,
    Railgun,
}

impl Disclosure {
    const fn header_id(self) -> &'static str {
        match self {
            Self::Advanced => "chain-advanced-header",
            Self::Railgun => "chain-railgun-header",
        }
    }
}

enum FieldInput {
    Single(Entity<InputState>),
    /// One input per URL. The wire value stays a newline-joined string.
    List(Vec<Entity<InputState>>),
}

impl FieldInput {
    fn value(&self, cx: &App) -> String {
        match self {
            Self::Single(input) => input.read(cx).value().to_string(),
            Self::List(rows) => rows
                .iter()
                .map(|row| row.read(cx).value().trim().to_owned())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl ChainEditor {
    /// Creates a list. Draft inputs are allocated only when a user opens or adds a chain.
    pub fn new(snapshot: ChainEditorSnapshot, cx: &mut Context<'_, Self>) -> Self {
        Self {
            snapshot,
            draft: None,
            opened_draft: None,
            pricing_select: None,
            pricing_status: BTreeMap::new(),
            pricing_preset: None,
            probe: None,
            probe_source: None,
            chain_id: None,
            fields: BTreeMap::new(),
            existing: false,
            pending: false,
            stale: false,
            advanced_open: false,
            railgun_open: false,
            edit_deployment: false,
            error: None,
            focus: cx.focus_handle(),
        }
    }

    /// True while a draft is open, so a host can route back navigation to the editor.
    #[must_use]
    pub const fn is_editing(&self) -> bool {
        self.draft.is_some()
    }

    /// Applies only a reply admitted by the host's current view/request authority.
    pub fn receive(
        &mut self,
        result: Result<ChainEditorSnapshot, String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending = false;
        self.clear_probe();
        match result {
            Ok(mut snapshot) => {
                self.error = None;
                self.stale = false;
                let draft = snapshot.draft.take();
                self.snapshot = snapshot;
                if let Some(draft) = draft {
                    self.edit(draft, true, window, cx);
                } else {
                    self.clear_draft();
                    self.focus.focus(window, cx);
                }
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    /// Adopts a host-side commit. An open draft keeps its edits and is marked stale instead.
    pub fn refresh(&mut self, snapshot: ChainEditorSnapshot, cx: &mut Context<'_, Self>) {
        if self.draft.is_some() && self.snapshot.revision != snapshot.revision {
            self.stale = true;
        }
        self.snapshot.revision = snapshot.revision;
        self.snapshot.chains = snapshot.chains;
        cx.notify();
    }

    /// Applies one unsaved Test reply. It never touches the draft, snapshot or saved status.
    pub fn receive_probe(&mut self, probe: NativeUsdProbe, cx: &mut Context<'_, Self>) {
        self.pending = false;
        self.probe = Some(probe);
        cx.notify();
    }

    /// Updates saved-source feedback without touching draft inputs or settings revision.
    pub fn set_pricing_status(
        &mut self,
        status: BTreeMap<String, NativeUsdStatus>,
        cx: &mut Context<'_, Self>,
    ) {
        self.pricing_status = status;
        cx.notify();
    }

    /// Retires credential-bearing data on lock, disconnect, revocation, or view closure.
    pub fn retire(&mut self, cx: &mut Context<'_, Self>) {
        self.clear_draft();
        self.snapshot = ChainEditorSnapshot::default();
        self.pricing_status.clear();
        self.pending = false;
        self.stale = false;
        self.error = None;
        cx.notify();
    }

    /// Inspect an existing chain through the same host path as the editor's chain list.
    pub fn inspect_chain(&mut self, chain_id: u64, cx: &mut Context<'_, Self>) {
        self.send(
            ChainEditorCommand::Inspect {
                chain_id: chain_id.to_string(),
            },
            cx,
        );
    }

    fn clear_probe(&mut self) {
        self.probe = None;
        self.probe_source = None;
    }

    fn clear_draft(&mut self) {
        self.clear_probe();
        self.draft = None;
        self.opened_draft = None;
        self.pricing_select = None;
        self.pricing_preset = None;
        self.chain_id = None;
        self.fields.clear();
        self.existing = false;
    }

    fn edit(
        &mut self,
        draft: ChainDraft,
        existing: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.opened_draft = Some(draft.clone());
        self.pricing_preset = draft
            .defaults
            .get(&ChainField::NativeUsdOracle)
            .map(|value| new_input(value, window, cx));
        let has_preset = draft.defaults.contains_key(&ChainField::NativeUsdOracle);
        let items = pricing_items(has_preset, draft.value(ChainField::NativeSymbol));
        let selected = if !has_preset && draft.native_usd_pricing == NativeUsdChoice::Disabled {
            NativeUsdChoice::Default
        } else {
            draft.native_usd_pricing
        };
        let index = items
            .iter()
            .position(|item| item.choice == selected)
            .map(gpui_component::IndexPath::new);
        let select = cx.new(|cx| SelectState::new(items, index, window, cx));
        cx.subscribe(&select, |this, _, event, cx| {
            if let SelectEvent::Confirm(Some(choice)) = event {
                if let Some(draft) = this.draft.as_mut() {
                    draft.native_usd_pricing = *choice;
                }
                this.clear_probe();
                cx.notify();
            }
        })
        .detach();
        self.pricing_select = Some(select);
        self.fields = ChainField::ALL
            .iter()
            .copied()
            .filter(|field| draft.railgun || !field.is_railgun())
            .map(|field| {
                let value = draft.value(field);
                let input = if field.is_url_list() {
                    FieldInput::List(
                        value
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(|line| new_input(line, window, cx))
                            .collect(),
                    )
                } else {
                    FieldInput::Single(new_input(value, window, cx))
                };
                (field, input)
            })
            .collect();
        let chain_id = new_input(&draft.chain_id, window, cx);
        if existing {
            // The row that opened this editor is gone, so keep focus on the editor root.
            self.focus.focus(window, cx);
        } else {
            chain_id.update(cx, |input, cx| input.focus(window, cx));
        }
        self.chain_id = Some(chain_id);
        let has_railgun = draft.railgun;
        self.draft = Some(draft);
        self.existing = existing;
        self.error = None;
        let advanced_open = self.override_count(ChainField::is_advanced_evm, cx) > 0;
        let railgun_open = has_railgun && self.railgun_override_count(cx) > 0;
        // Existing deployment overrides stay editable; untouched presets start locked.
        let edit_deployment = ChainField::ALL
            .iter()
            .copied()
            .filter(|field| field.is_deployment())
            .any(|field| self.changed_default(field, cx).is_some());
        self.advanced_open = advanced_open;
        self.railgun_open = railgun_open;
        self.edit_deployment = edit_deployment;
        cx.notify();
    }

    fn send(&mut self, command: ChainEditorCommand, cx: &mut Context<'_, Self>) {
        if self.pending || self.snapshot.revision.is_empty() {
            return;
        }
        self.pending = true;
        self.error = None;
        self.clear_probe();
        cx.emit(ChainEditorEvent {
            revision: self.snapshot.revision.clone(),
            command,
        });
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<'_, Self>) {
        let Some(draft) = self.collected_draft(cx) else {
            return;
        };
        self.send(
            ChainEditorCommand::Save {
                draft,
                existing: self.existing,
            },
            cx,
        );
    }

    /// The open draft with every input's current value, as the host receives it.
    fn collected_draft(&self, cx: &App) -> Option<ChainDraft> {
        let mut draft = self.draft.clone()?;
        if let Some(input) = self.chain_id.as_ref() {
            draft.chain_id = input.read(cx).value().to_string();
        }
        for (&field, input) in &self.fields {
            draft.fields.insert(field, input.value(cx));
        }
        Some(draft)
    }

    /// Reads the draft's own source once. Save stays available whatever the result is.
    fn start_probe(&mut self, cx: &mut Context<'_, Self>) {
        let Some(source) = self.draft_pricing_source(cx) else {
            return;
        };
        let Some(draft) = self.collected_draft(cx) else {
            return;
        };
        self.send(ChainEditorCommand::Probe { draft }, cx);
        self.probe_source = Some(source);
    }

    /// The address the draft currently resolves to, or none when it has no source.
    fn draft_pricing_source(&self, cx: &App) -> Option<String> {
        let source = match self.draft.as_ref()?.native_usd_pricing {
            NativeUsdChoice::Oracle => self.fields.get(&ChainField::NativeUsdOracle)?.value(cx),
            NativeUsdChoice::Default => self.field_default(ChainField::NativeUsdOracle)?.to_owned(),
            NativeUsdChoice::Disabled => return None,
        };
        let source = source.trim().to_owned();
        (!source.is_empty()).then_some(source)
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.clear_draft();
        self.focus.focus(window, cx);
        self.send(ChainEditorCommand::List, cx);
    }

    fn apply_default(
        &self,
        field: ChainField,
        value: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(FieldInput::Single(input)) = self.fields.get(&field) else {
            return;
        };
        let input = input.clone();
        input.update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
    }

    fn move_url(&mut self, field: ChainField, index: usize, up: bool, cx: &mut Context<'_, Self>) {
        if let Some(FieldInput::List(rows)) = self.fields.get_mut(&field) {
            let len = rows.len();
            let target = if up {
                index.checked_sub(1)
            } else {
                index.checked_add(1)
            };
            if let Some(target) = target.filter(|target| *target < len) {
                rows.swap(index, target);
            }
        }
        cx.notify();
    }

    fn remove_url(&mut self, field: ChainField, index: usize, cx: &mut Context<'_, Self>) {
        if let Some(FieldInput::List(rows)) = self.fields.get_mut(&field)
            && index < rows.len()
        {
            rows.remove(index);
        }
        cx.notify();
    }

    fn add_url(&mut self, field: ChainField, window: &mut Window, cx: &mut Context<'_, Self>) {
        let input = new_input("", window, cx);
        if let Some(FieldInput::List(rows)) = self.fields.get_mut(&field) {
            rows.push(input.clone());
        }
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn field_default(&self, field: ChainField) -> Option<&str> {
        self.draft
            .as_ref()?
            .defaults
            .get(&field)
            .map(String::as_str)
    }

    /// The inherited value, when the current input no longer matches it.
    fn changed_default(&self, field: ChainField, cx: &App) -> Option<SharedString> {
        let default = self.field_default(field)?;
        let current = self.fields.get(&field)?.value(cx);
        (current.trim() != default.trim()).then(|| SharedString::from(default.to_owned()))
    }

    fn override_count(&self, group: fn(ChainField) -> bool, cx: &App) -> usize {
        ChainField::ALL
            .iter()
            .copied()
            .filter(|&field| group(field) && self.changed_default(field, cx).is_some())
            .count()
    }

    /// Railgun counts the two preset toggles instead of the relay list they control.
    fn railgun_override_count(&self, cx: &App) -> usize {
        let Some(draft) = self.draft.as_ref() else {
            return 0;
        };
        let fields = ChainField::ALL
            .iter()
            .copied()
            .filter(|&field| {
                field.is_railgun()
                    && field != ChainField::SponsoredBundleRelays
                    && self.changed_default(field, cx).is_some()
            })
            .count();
        fields + usize::from(!draft.quick_sync_enabled) + usize::from(!draft.use_default_relays)
    }

    fn render_list(&self, root: gpui::Div, cx: &Context<'_, Self>) -> gpui::Div {
        let unavailable = self.pending || self.snapshot.revision.is_empty();
        let (built_in, custom): (Vec<_>, Vec<_>) = self
            .snapshot
            .chains
            .iter()
            .partition(|chain| chain.built_in);
        let (railgun, public): (Vec<_>, Vec<_>) =
            built_in.into_iter().partition(|chain| chain.railgun);
        root.gap_3()
            .p_3()
            .when(self.stale, |body| {
                body.child(
                    Alert::warning(
                        "chain-list-stale",
                        "Chain settings changed in another window.",
                    )
                    .small(),
                )
            })
            .children(
                self.error
                    .as_ref()
                    .map(|error| Alert::error("chain-list-error", error.clone()).small()),
            )
            .child(
                div().flex().justify_end().child(
                    app_button("chain-add", "Add chain")
                        .icon(IconName::Plus)
                        .disabled(unavailable)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.edit(ChainDraft::new(), false, window, cx);
                        })),
                ),
            )
            .when(!railgun.is_empty(), |body| {
                body.child(Self::render_group(
                    "Railgun",
                    "Private and public",
                    &railgun,
                    unavailable,
                    cx,
                ))
            })
            .when(!public.is_empty(), |body| {
                body.child(Self::render_group(
                    "Public only",
                    "Built-in EVM chains",
                    &public,
                    unavailable,
                    cx,
                ))
            })
            .child(Self::render_custom_group(&custom, unavailable, cx))
    }

    fn render_group(
        title: &str,
        hint: &'static str,
        chains: &[&ChainSummary],
        unavailable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        chains.iter().fold(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .min_w_0()
                .child(group_header(title, chains.len(), hint)),
            |group, &chain| group.child(Self::render_row(chain, unavailable, cx)),
        )
    }

    fn render_custom_group(
        chains: &[&ChainSummary],
        unavailable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        if !chains.is_empty() {
            return Self::render_group("Custom", "Public only", chains, unavailable, cx);
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .child(group_header("Custom", 0, "Public only"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_dashed()
                    .border_color(cx.theme().border)
                    .child(
                        app_muted_text(
                            "No custom chains yet. Add any EVM chain by chain ID and RPC endpoint. \
                             Custom chains are public only: no private balances and no Shield.",
                        )
                        .whitespace_normal(),
                    )
                    .child(
                        app_button("chain-add-empty", "Add chain")
                            .icon(IconName::Plus)
                            .small()
                            .disabled(unavailable)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.edit(ChainDraft::new(), false, window, cx);
                            })),
                    ),
            )
    }

    fn render_row(chain: &ChainSummary, unavailable: bool, cx: &Context<'_, Self>) -> Button {
        let id = chain.chain_id.clone();
        let selector = format!("edit-chain-{id}");
        let icon = id
            .parse::<u64>()
            .ok()
            .and_then(railgun_ui::chain_icon_asset_path);
        let dim = !chain.enabled;
        let mut tags: Vec<AnyElement> = Vec::new();
        if !chain.built_in {
            tags.push(
                Tag::secondary()
                    .small()
                    .rounded_full()
                    .child("Public only")
                    .into_any_element(),
            );
        }
        if chain.modified {
            tags.push(
                Tag::warning()
                    .small()
                    .rounded_full()
                    .child("Modified")
                    .into_any_element(),
            );
        }
        if dim {
            tags.push(
                Tag::secondary()
                    .outline()
                    .small()
                    .rounded_full()
                    .child("Disabled")
                    .into_any_element(),
            );
        }
        let endpoints = chain.rpc_endpoints;
        let meta = div()
            .flex()
            .items_center()
            .gap_1()
            .min_w_0()
            .text_size(META_TEXT_SIZE)
            .line_height(META_LINE_HEIGHT)
            .text_color(rgb(if dim {
                theme::TEXT_SUBTLE
            } else {
                theme::TEXT_MUTED
            }))
            .child("Chain ID")
            .child(
                div()
                    .flex_none()
                    .font_family(APP_MONO_FONT_FAMILY)
                    .child(SharedString::from(id.clone())),
            )
            .child(SharedString::from(format!(
                "· {endpoints} RPC endpoint{}",
                if endpoints == 1 { "" } else { "s" }
            )));
        Button::new(SharedString::from(selector.clone()))
            .ghost()
            .w_full()
            .h_auto()
            .min_h(px(44.0))
            .px_2()
            .py(px(6.0))
            .debug_selector(move || selector)
            .accessibility_label(format!("Edit {}", chain.name))
            .disabled(unavailable)
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(chain_avatar(&chain.name, icon, ROW_ICON_SIZE, dim, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .min_w_0()
                                    .child(
                                        app_text(chain.name.clone())
                                            .min_w_0()
                                            .truncate()
                                            .text_color(rgb(if dim {
                                                theme::TEXT_SUBTLE
                                            } else {
                                                theme::TEXT
                                            })),
                                    )
                                    .children(tags),
                            )
                            .child(meta),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .xsmall()
                            .flex_none()
                            .text_color(rgb(theme::TEXT_SUBTLE)),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.send(
                    ChainEditorCommand::Inspect {
                        chain_id: id.clone(),
                    },
                    cx,
                );
            }))
    }

    fn render_editor(
        &self,
        root: gpui::Div,
        draft: &ChainDraft,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        root.size_full()
            .min_h_0()
            .child(
                div()
                    .id("chain-editor-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_editor_body(draft, cx)),
            )
            .child(self.render_footer(draft, cx))
    }

    fn render_editor_body(&self, draft: &ChainDraft, cx: &Context<'_, Self>) -> gpui::Div {
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .min_w_0()
            .child(
                div().flex().items_center().child(
                    Button::new("chain-back")
                        .ghost()
                        .xsmall()
                        .compact()
                        .icon(IconName::ChevronLeft)
                        .label("Chains")
                        .accessibility_label("Back to chains")
                        .disabled(self.pending)
                        .on_click(cx.listener(|this, _, window, cx| this.discard(window, cx))),
                ),
            )
            .child(self.render_editor_header(draft, cx))
            .child(if draft.built_in {
                Alert::info(
                    "chain-built-in",
                    if draft.railgun {
                        "Built-in chain. Name, currency and explorer come from the preset. \
                         Only values you change are saved."
                    } else {
                        "Built-in chain. Name, currency and explorer come from the preset. \
                         Only values you change are saved. Public only: private balances and \
                         Shield are unavailable on this chain."
                    },
                )
                .small()
            } else {
                Alert::info(
                    "chain-public-only",
                    "Public only. Private balances and Shield are unavailable on this chain.",
                )
                .small()
            });
        if self.stale {
            body = body.child(
                Alert::warning(
                    "chain-stale",
                    "This chain was changed elsewhere after you started editing. \
                     Your edits are kept; review the latest values before saving.",
                )
                .small(),
            );
        }
        if !draft.built_in {
            body = body.child(self.render_identity_section(cx));
        }
        body = body
            .child(self.render_endpoints_section(cx))
            .child(self.render_pricing_section(draft, cx))
            .child(self.render_advanced_section(cx));
        if draft.railgun {
            body = body.child(self.render_railgun_section(draft, cx));
        }
        body
    }

    fn render_editor_header(&self, draft: &ChainDraft, cx: &Context<'_, Self>) -> gpui::Div {
        let name = draft.value(ChainField::Name);
        let title = if self.existing && !name.is_empty() {
            name.to_owned()
        } else {
            "New chain".to_owned()
        };
        let capability = if draft.railgun {
            "Railgun"
        } else {
            "Public only"
        };
        let subtitle = if self.existing {
            format!("Chain ID {} · {capability}", draft.chain_id)
        } else {
            capability.to_owned()
        };
        let icon = if draft.built_in {
            draft
                .chain_id
                .parse::<u64>()
                .ok()
                .and_then(railgun_ui::chain_icon_asset_path)
        } else {
            None
        };
        let avatar = chain_avatar(
            if self.existing { title.as_str() } else { "?" },
            icon,
            HEADER_ICON_SIZE,
            false,
            cx,
        );
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_w_0()
            .child(avatar)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(app_strong_text(title).min_w_0().truncate())
                    .child(meta_text(subtitle)),
            )
            .child(
                Checkbox::new("chain-enabled")
                    .debug_selector(|| "chain-enabled".into())
                    .label("Enabled")
                    .checked(draft.enabled)
                    .disabled(self.pending)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        if let Some(draft) = this.draft.as_mut() {
                            draft.enabled = *checked;
                        }
                        cx.notify();
                    })),
            )
    }

    /// Custom chains define their own identity. Built-in identity comes from the preset.
    fn render_identity_section(&self, cx: &Context<'_, Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .children(self.chain_id.as_ref().map(|input| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(meta_text("Chain ID"))
                    .child(
                        app_input(input)
                            .aria_label("Chain ID")
                            .readonly(self.existing)
                            .disabled(self.pending),
                    )
                    .children((!self.existing).then(|| subtle_text("Cannot be changed later.")))
            }))
            .child(self.render_field(ChainField::Name, false, None, cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .min_w_0()
                    .child(
                        self.render_field(ChainField::NativeName, false, None, cx)
                            .flex_1()
                            .min_w(px(120.0)),
                    )
                    .child(
                        self.render_field(ChainField::NativeSymbol, false, None, cx)
                            .min_w(px(120.0)),
                    )
                    .child(
                        self.render_field(ChainField::NativeDecimals, false, None, cx)
                            .min_w(px(120.0)),
                    ),
            )
            .child(self.render_url_list(
                ChainField::ExplorerUrls,
                false,
                Some("Optional. Used for transaction links."),
                cx,
            ))
    }

    fn render_endpoints_section(&self, cx: &Context<'_, Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(section_title("RPC ENDPOINTS"))
                    .child(subtle_text("Preference order")),
            )
            .child(self.render_url_list(
                ChainField::RpcEndpoints,
                false,
                Some("Endpoints are checked against the chain ID before use."),
                cx,
            ))
    }

    fn render_advanced_section(&self, cx: &Context<'_, Self>) -> Collapsible {
        let count = self.override_count(ChainField::is_advanced_evm, cx);
        let trailing = if count > 0 {
            override_label(count)
        } else if self.existing {
            "Defaults".to_owned()
        } else {
            "Optional".to_owned()
        };
        let content = ChainField::ALL
            .iter()
            .copied()
            .filter(|field| field.is_advanced_evm())
            .fold(section_content(), |body, field| {
                body.child(self.render_field(field, false, None, cx))
            });
        Collapsible::new()
            .w_full()
            .open(self.advanced_open)
            .child(self.section_header(Disclosure::Advanced, "Advanced EVM", trailing, cx))
            .content(content)
    }

    fn render_railgun_section(&self, draft: &ChainDraft, cx: &Context<'_, Self>) -> Collapsible {
        let count = self.railgun_override_count(cx);
        let trailing = if count > 0 {
            override_label(count)
        } else {
            "Defaults".to_owned()
        };
        let content = section_content()
            .child(
                Checkbox::new("chain-quick-sync")
                    .label("Enable quick-sync")
                    .checked(draft.quick_sync_enabled)
                    .disabled(self.pending)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        if let Some(draft) = this.draft.as_mut() {
                            draft.quick_sync_enabled = *checked;
                        }
                        cx.notify();
                    })),
            )
            .child(self.render_field(ChainField::QuickSyncEndpoint, false, None, cx))
            .child(self.render_field(
                ChainField::QuickSyncIndexedWalletBlockRange,
                false,
                None,
                cx,
            ))
            .child(self.render_field(ChainField::BlockRange, false, None, cx))
            .child(self.render_field(ChainField::PollIntervalSecs, false, None, cx))
            .child(self.render_field(ChainField::IndexedWalletBlockRange, false, None, cx))
            .child(
                Checkbox::new("chain-default-relays")
                    .label("Use preset sponsored relays")
                    .checked(draft.use_default_relays)
                    .disabled(self.pending)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        if let Some(draft) = this.draft.as_mut() {
                            draft.use_default_relays = *checked;
                        }
                        cx.notify();
                    })),
            )
            .child(self.render_url_list(
                ChainField::SponsoredBundleRelays,
                draft.use_default_relays,
                None,
                cx,
            ))
            .child(self.render_deployment_block(cx));
        Collapsible::new()
            .w_full()
            .open(self.railgun_open)
            .child(self.section_header(Disclosure::Railgun, "Railgun", trailing, cx))
            .content(content)
    }

    fn render_deployment_block(&self, cx: &Context<'_, Self>) -> gpui::Div {
        ChainField::ALL
            .iter()
            .copied()
            .filter(|field| field.is_deployment())
            .fold(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .min_w_0()
                    .child(
                        Alert::warning(
                            "chain-deployment-warning",
                            "Contract addresses and deployment blocks can make funds unreachable \
                             if changed.",
                        )
                        .small(),
                    )
                    .child(
                        Checkbox::new("chain-edit-deployment")
                            .label("Edit deployment values")
                            .checked(self.edit_deployment)
                            .disabled(self.pending)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.edit_deployment = *checked;
                                cx.notify();
                            })),
                    ),
                |body, field| body.child(self.render_field(field, !self.edit_deployment, None, cx)),
            )
    }

    const fn disclosure_open(&self, section: Disclosure) -> bool {
        match section {
            Disclosure::Advanced => self.advanced_open,
            Disclosure::Railgun => self.railgun_open,
        }
    }

    fn section_header(
        &self,
        section: Disclosure,
        title: &str,
        trailing: String,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.disclosure_open(section);
        div()
            .id(section.header_id())
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px(px(10.0))
            .py(px(3.0))
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(rgb(theme::SURFACE))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let open = match section {
                    Disclosure::Advanced => &mut this.advanced_open,
                    Disclosure::Railgun => &mut this.railgun_open,
                };
                *open = !*open;
                cx.notify();
            }))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED)),
            )
            .child(section_title(title.to_ascii_uppercase()))
            .child(subtle_text(trailing))
    }

    fn render_field(
        &self,
        field: ChainField,
        readonly: bool,
        help: Option<&'static str>,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let Some(FieldInput::Single(input)) = self.fields.get(&field) else {
            return div();
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .child(self.render_field_label(field, cx))
            .child(
                app_input(input)
                    .aria_label(field.label())
                    .readonly(readonly)
                    .disabled(self.pending),
            )
            .children(help.map(|text| subtle_text(text).whitespace_normal()))
    }

    fn render_field_label(&self, field: ChainField, cx: &Context<'_, Self>) -> gpui::Div {
        let row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .min_w_0()
            .child(meta_text(field.label()).flex_1().min_w_0().truncate());
        let Some(default) = self.changed_default(field, cx) else {
            return row;
        };
        row.child(
            Button::new(("chain-use-default", field as usize))
                .ghost()
                .xsmall()
                .compact()
                .label("Use default")
                .accessibility_label(format!("Use default {}", field.label()))
                .disabled(self.pending)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.apply_default(field, default.clone(), window, cx);
                })),
        )
    }

    fn render_url_list(
        &self,
        field: ChainField,
        readonly: bool,
        help: Option<&'static str>,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let Some(FieldInput::List(rows)) = self.fields.get(&field) else {
            return div();
        };
        let key = field as usize;
        let noun = url_noun(field);
        let last = rows.len().saturating_sub(1);
        let mut list = div().flex().flex_col().gap_2().min_w_0();
        for (index, input) in rows.iter().enumerate() {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div().flex_1().min_w_0().child(
                            app_input(input)
                                .aria_label(field.label())
                                .readonly(readonly)
                                .disabled(self.pending),
                        ),
                    )
                    .child(row_icon_button(
                        SharedString::from(format!("chain-url-{key}-{index}-up")),
                        IconName::ArrowUp,
                        "Move up".into(),
                        readonly || self.pending || index == 0,
                        cx.listener(move |this, _, _, cx| this.move_url(field, index, true, cx)),
                    ))
                    .child(row_icon_button(
                        SharedString::from(format!("chain-url-{key}-{index}-down")),
                        IconName::ArrowDown,
                        "Move down".into(),
                        readonly || self.pending || index == last,
                        cx.listener(move |this, _, _, cx| this.move_url(field, index, false, cx)),
                    ))
                    .child(row_icon_button(
                        SharedString::from(format!("chain-url-{key}-{index}-remove")),
                        IconName::Close,
                        SharedString::from(format!("Remove {noun}")),
                        readonly || self.pending,
                        cx.listener(move |this, _, _, cx| this.remove_url(field, index, cx)),
                    )),
            );
        }
        list.child(
            div().flex().child(
                app_button(("chain-add-url", key), format!("Add {noun}"))
                    .icon(IconName::Plus)
                    .small()
                    .disabled(readonly || self.pending)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_url(field, window, cx);
                    })),
            ),
        )
        .children(help.map(|text| subtle_text(text).whitespace_normal()))
    }

    fn only_pricing_differs(&self, draft: &ChainDraft, cx: &App) -> bool {
        let Some(opened) = &self.opened_draft else {
            return false;
        };
        draft.enabled == opened.enabled
            && draft.quick_sync_enabled == opened.quick_sync_enabled
            && draft.use_default_relays == opened.use_default_relays
            && self.fields.iter().all(|(&field, input)| {
                field == ChainField::NativeUsdOracle || input.value(cx) == opened.value(field)
            })
    }

    fn render_pricing_section(&self, draft: &ChainDraft, cx: &Context<'_, Self>) -> gpui::Div {
        let status = self
            .pricing_status
            .get(&draft.chain_id)
            .cloned()
            .unwrap_or_default();
        let mut section = div()
            .flex()
            .flex_col()
            .gap_2()
            .min_w_0()
            .child(section_title("USD PRICE"));
        if let Some(select) = &self.pricing_select {
            section = section.child(
                Select::new(select)
                    .accessibility_label("Native USD source")
                    .disabled(self.pending)
                    .w_full(),
            );
        }
        let source = self.draft_pricing_source(cx);
        match draft.native_usd_pricing {
            NativeUsdChoice::Oracle => {
                if let Some(FieldInput::Single(input)) =
                    self.fields.get(&ChainField::NativeUsdOracle)
                {
                    section = section
                        .child(self.render_field_label(ChainField::NativeUsdOracle, cx))
                        .child(
                            app_input_group(
                                "chain-pricing-oracle",
                                input,
                                ChainField::NativeUsdOracle.label(),
                            )
                            .disabled(self.pending)
                            .addon(self.render_test_addon(source.is_none(), cx)),
                        )
                        .child(subtle_text("Use a Chainlink-compatible feed on this chain, quoting USD per whole native coin.").whitespace_normal());
                }
            }
            NativeUsdChoice::Default => {
                if let Some(preset) = &self.pricing_preset {
                    section = section
                        .child(
                            app_input_group("chain-pricing-preset", preset, "Preset oracle")
                                .readonly(true)
                                .addon(self.render_test_addon(source.is_none(), cx)),
                        )
                        .child(
                            subtle_text("Inherits this chain's preset USD feed.")
                                .whitespace_normal(),
                        );
                } else {
                    section = section.child(
                        subtle_text("No preset feed. Native amounts have no USD value.")
                            .whitespace_normal(),
                    );
                }
            }
            NativeUsdChoice::Disabled => {
                section =
                    section.child(subtle_text("Native USD pricing is off.").whitespace_normal());
            }
        }
        if let Some(probe) = self
            .probe
            .as_ref()
            .filter(|_| self.probe_source.is_some() && self.probe_source == source)
        {
            section = section.child(self.render_probe_alert(probe, cx));
        }
        let quote = status.quote.as_ref().map(|quote| {
            let price = quote
                .micro_usd
                .parse::<ruint::aliases::U256>()
                .ok()
                .map_or_else(|| "Unavailable".into(), railgun_ui::format_usd_micro_value);
            let time = i64::try_from(quote.obtained_at)
                .ok()
                .and_then(|time| chrono::DateTime::from_timestamp(time, 0))
                .map_or_else(
                    || "Unknown time".into(),
                    |time| time.format("%Y-%m-%d %H:%M UTC").to_string(),
                );
            format!(
                "{price} per {}. Updated {time}.",
                self.opened_draft
                    .as_ref()
                    .map_or("native coin", |draft| draft.value(ChainField::NativeSymbol))
            )
        });
        let detail = [quote, status.reason]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let alert = match status.state {
            NativeUsdState::Unconfigured => {
                Alert::new("native-usd-status", "No active source").title("Saved pricing")
            }
            NativeUsdState::Pending => Alert::info(
                "native-usd-status",
                if detail.is_empty() {
                    "Waiting for the oracle".into()
                } else {
                    detail
                },
            )
            .title("Saved pricing: pending"),
            NativeUsdState::Available => {
                Alert::success("native-usd-status", detail).title("Saved pricing: available")
            }
            NativeUsdState::Failed if status.quote.is_some() => Alert::warning(
                "native-usd-status",
                format!("Previous price remains in use. {detail}"),
            )
            .title("Saved pricing: refresh failed"),
            NativeUsdState::Failed => {
                Alert::error("native-usd-status", detail).title("Saved pricing: unavailable")
            }
        };
        section.child(alert.small())
    }

    /// The trailing Test action shared by the oracle and preset rows.
    fn render_test_addon(&self, empty: bool, cx: &Context<'_, Self>) -> InputGroupAddon {
        // A save sets `pending` too, so only a started test without its reply spins.
        let testing = self.pending && self.probe_source.is_some() && self.probe.is_none();
        InputGroupAddon::new("chain-pricing-test-addon")
            .align(InputGroupAddonAlignment::InlineEnd)
            .child(
                InputGroupButton::new("chain-pricing-test")
                    .label("Test")
                    .debug_selector(|| "chain-pricing-test".into())
                    .disabled(self.pending || empty)
                    .loading(testing)
                    // The spinner takes the icon slot, so the slot has to exist while
                    // the test runs. This icon itself is never drawn.
                    .when(testing, |button| button.icon(IconName::Loader))
                    .on_click(cx.listener(|this, _, _, cx| this.start_probe(cx))),
            )
    }

    /// Unsaved feedback for the draft's source. It never becomes saved pricing status.
    fn render_probe_alert(&self, probe: &NativeUsdProbe, cx: &App) -> Alert {
        let alert = match (&probe.quote, &probe.message) {
            (Some(quote), _) => {
                let price = quote
                    .micro_usd
                    .parse::<ruint::aliases::U256>()
                    .ok()
                    .map_or_else(|| "Unavailable".into(), railgun_ui::format_usd_micro_value);
                let symbol = self
                    .fields
                    .get(&ChainField::NativeSymbol)
                    .map(|input| input.value(cx))
                    .filter(|symbol| !symbol.trim().is_empty())
                    .unwrap_or_else(|| "native coin".to_owned());
                Alert::success("native-usd-probe", format!("{price} per {symbol}."))
            }
            (None, Some(message)) => Alert::error("native-usd-probe", message.clone()),
            (None, None) => Alert::error("native-usd-probe", "The test did not return a price."),
        };
        alert.title("Test result").small()
    }

    fn render_footer(&self, draft: &ChainDraft, cx: &Context<'_, Self>) -> gpui::Div {
        let unavailable = self.pending || self.snapshot.revision.is_empty();
        let mut footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap_2()
            .p_3()
            .flex_none()
            .border_t_1()
            .border_color(cx.theme().border)
            .children(self.error.as_ref().map(|error| {
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .text_size(META_TEXT_SIZE)
                    .line_height(META_LINE_HEIGHT)
                    .text_color(rgb(theme::DANGER))
                    .whitespace_normal()
                    .child(SharedString::from(error.clone()))
            }));
        if self.existing {
            footer = footer.child(if draft.built_in {
                Self::reset_button(draft, unavailable, cx)
            } else {
                Self::remove_button(draft, unavailable, cx)
            });
        }
        footer
            .child(
                app_button("chain-discard", "Discard")
                    .debug_selector(|| "chain-discard".into())
                    .disabled(self.pending)
                    .on_click(cx.listener(|this, _, window, cx| this.discard(window, cx))),
            )
            .child(
                app_button(
                    "chain-save",
                    if draft.built_in && !self.only_pricing_differs(draft, cx) {
                        "Save and restart"
                    } else if self.existing {
                        "Save"
                    } else {
                        "Add chain"
                    },
                )
                .primary()
                .debug_selector(|| "chain-save".into())
                .disabled(unavailable || self.stale)
                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
    }

    fn reset_button(draft: &ChainDraft, unavailable: bool, cx: &Context<'_, Self>) -> Button {
        let chain_id = draft.chain_id.clone();
        let name = display_name(draft);
        app_button("chain-reset", "Reset to preset")
            .ghost()
            .debug_selector(|| "chain-reset".into())
            .disabled(unavailable)
            .on_click(cx.listener(move |_, _, window, cx| {
                let editor = cx.entity();
                let chain_id = chain_id.clone();
                let name = name.clone();
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let editor = editor.clone();
                    let chain_id = chain_id.clone();
                    dialog
                        .width(CONFIRM_DIALOG_WIDTH)
                        .title(app_strong_text(format!("Reset \"{name}\" to preset?")))
                        .child(
                            app_muted_text(
                                "Removes this chain's overrides. Networking restarts if operational settings change.",
                            )
                            .whitespace_normal(),
                        )
                        .button_props(
                            DialogButtonProps::default()
                                .ok_text("Reset")
                                .ok_variant(ButtonVariant::Primary)
                                .cancel_variant(ButtonVariant::Secondary),
                        )
                        // After button_props, which would otherwise replace the Cancel flag.
                        .confirm()
                        .on_ok(move |_, _, cx| {
                            let chain_id = chain_id.clone();
                            editor.update(cx, |this, cx| {
                                this.send(ChainEditorCommand::Reset { chain_id }, cx);
                            });
                            true
                        })
                });
            }))
    }

    fn remove_button(draft: &ChainDraft, unavailable: bool, cx: &Context<'_, Self>) -> Button {
        let chain_id = draft.chain_id.clone();
        let name = display_name(draft);
        app_button("chain-remove", "Remove chain")
            .ghost()
            .text_color(cx.theme().danger)
            .debug_selector(|| "chain-remove".into())
            .disabled(unavailable)
            .on_click(cx.listener(move |_, _, window, cx| {
                let editor = cx.entity();
                let chain_id = chain_id.clone();
                let name = name.clone();
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let editor = editor.clone();
                    let chain_id = chain_id.clone();
                    let body = format!(
                        "Accounts, history and cached data on chain {chain_id} are kept and \
                         return if you add it again. Dapps connected to it lose it until you \
                         switch them."
                    );
                    dialog
                        .width(CONFIRM_DIALOG_WIDTH)
                        .title(app_strong_text(format!("Remove \"{name}\"?")))
                        .child(app_muted_text(body).whitespace_normal())
                        .button_props(
                            DialogButtonProps::default()
                                .ok_text("Remove")
                                .ok_variant(ButtonVariant::Danger)
                                .cancel_variant(ButtonVariant::Secondary),
                        )
                        // After button_props, which would otherwise replace the Cancel flag.
                        .confirm()
                        .on_ok(move |_, _, cx| {
                            let chain_id = chain_id.clone();
                            editor.update(cx, |this, cx| {
                                this.send(ChainEditorCommand::Remove { chain_id }, cx);
                            });
                            true
                        })
                });
            }))
    }
}

impl Render for ChainEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let root = div()
            .flex()
            .flex_col()
            .min_w_0()
            .line_height(relative(APP_TEXT_LINE_HEIGHT))
            .track_focus(&self.focus);
        if let Some(draft) = self.draft.as_ref() {
            self.render_editor(root, draft, cx)
        } else {
            self.render_list(root, cx)
        }
    }
}

fn new_input(
    value: &str,
    window: &mut Window,
    cx: &mut Context<'_, ChainEditor>,
) -> Entity<InputState> {
    let value = value.to_owned();
    let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
    cx.observe(&input, |_, _, cx| cx.notify()).detach();
    input
}

fn row_icon_button(
    id: SharedString,
    icon: IconName,
    label: SharedString,
    disabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .compact()
        .icon(icon)
        .accessibility_label(label.clone())
        .tooltip(label)
        .disabled(disabled)
        .on_click(on_click)
}

fn chain_avatar(
    name: &str,
    icon: Option<&'static str>,
    size: Pixels,
    dim: bool,
    cx: &App,
) -> gpui::Div {
    let letter = name
        .chars()
        .next()
        .map_or_else(|| "?".to_owned(), |first| first.to_uppercase().to_string());
    div()
        .size(size)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .when(dim, |slot| slot.opacity(0.5))
        .map(|slot| {
            if let Some(path) = icon {
                slot.child(img(path).size(size))
            } else {
                slot.rounded_full()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().secondary)
                    .text_size(size * 0.5)
                    .line_height(size)
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(letter))
            }
        })
}

fn section_content() -> gpui::Div {
    div().flex().flex_col().gap_3().min_w_0().pt(px(6.0))
}

fn group_header(title: &str, count: usize, hint: &'static str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(section_title(format!(
            "{} · {count}",
            title.to_ascii_uppercase()
        )))
        .child(subtle_text(hint))
}

fn section_title(label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .text_size(META_TEXT_SIZE)
        .line_height(META_LINE_HEIGHT)
        .font_family(APP_MONO_FONT_FAMILY)
        .text_color(rgb(theme::TEXT_MUTED))
        .child(label.into())
}

fn meta_text(label: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(META_TEXT_SIZE)
        .line_height(META_LINE_HEIGHT)
        .text_color(rgb(theme::TEXT_MUTED))
        .child(label.into())
}

fn subtle_text(label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .text_size(META_TEXT_SIZE)
        .line_height(META_LINE_HEIGHT)
        .text_color(rgb(theme::TEXT_SUBTLE))
        .child(label.into())
}

fn override_label(count: usize) -> String {
    format!("{count} override{}", if count == 1 { "" } else { "s" })
}

const fn url_noun(field: ChainField) -> &'static str {
    match field {
        ChainField::ExplorerUrls => "explorer",
        ChainField::SponsoredBundleRelays => "relay",
        _ => "endpoint",
    }
}

fn display_name(draft: &ChainDraft) -> String {
    let name = draft.value(ChainField::Name);
    if name.is_empty() {
        format!("Chain {}", draft.chain_id)
    } else {
        name.to_owned()
    }
}

#[derive(Clone)]
struct PricingItem {
    choice: NativeUsdChoice,
    label: SharedString,
    description: &'static str,
}

impl SelectItem for PricingItem {
    type Value = NativeUsdChoice;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &Self::Value {
        &self.choice
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(app_text(self.label.clone()))
            .child(app_muted_text(self.description))
    }
}

fn pricing_items(preset: bool, symbol: &str) -> Vec<PricingItem> {
    let mut items = vec![
        PricingItem {
            choice: NativeUsdChoice::Default,
            label: if preset {
                format!("{symbol} / USD (preset)").into()
            } else {
                "None".into()
            },
            description: if preset {
                "Use this chain's preset feed"
            } else {
                "No native USD pricing"
            },
        },
        PricingItem {
            choice: NativeUsdChoice::Oracle,
            label: "Custom oracle".into(),
            description: "A USD feed on this chain",
        },
    ];
    if preset {
        items.push(PricingItem {
            choice: NativeUsdChoice::Disabled,
            label: "Off".into(),
            description: "Disable native USD pricing",
        });
    }
    items
}

#[cfg(test)]
mod pricing_tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    #[gpui::test]
    fn saved_pricing_updates_preserve_keyboard_selected_source_and_unsaved_inputs(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let mut editor = None;
        let handle = cx.add_window(|window, cx| {
            let mut snapshot = ChainEditorSnapshot::default();
            snapshot.revision = "saved-revision".into();
            let view = cx.new(|cx| ChainEditor::new(snapshot, cx));
            view.update(cx, |editor, cx| {
                let mut draft = ChainDraft::new();
                draft.chain_id = "1".into();
                draft.built_in = true;
                draft.railgun = true;
                draft.fields.insert(ChainField::NativeSymbol, "ETH".into());
                draft.fields.insert(ChainField::Name, "Ethereum".into());
                draft.fields.insert(
                    ChainField::NativeUsdOracle,
                    "0x0000000000000000000000000000000000000001".into(),
                );
                draft.defaults.insert(
                    ChainField::NativeUsdOracle,
                    draft.value(ChainField::NativeUsdOracle).into(),
                );
                editor.edit(draft, true, window, cx);
            });
            editor = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let editor = editor.unwrap();
        let cx = VisualTestContext::from_window(*handle, cx).into_mut();
        cx.simulate_resize(gpui::size(px(360.0), px(480.0)));
        cx.update(|window, cx| {
            let select = editor.read(cx).pricing_select.clone().unwrap();
            select.update(cx, |select, cx| select.focus(window, cx));
        });
        cx.simulate_keystrokes("enter down enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert_eq!(
                    editor.draft.as_ref().unwrap().native_usd_pricing,
                    NativeUsdChoice::Oracle
                );
                let FieldInput::Single(input) = &editor.fields[&ChainField::NativeUsdOracle] else {
                    unreachable!()
                };
                input.update(cx, |input, cx| {
                    input.set_value("unfinished oracle", window, cx);
                });
                let mut status = NativeUsdStatus::default();
                status.state = NativeUsdState::Failed;
                status.reason = Some("Check the oracle contract".into());
                editor.set_pricing_status(BTreeMap::from([("1".into(), status)]), cx);
                assert_eq!(
                    editor.fields[&ChainField::NativeUsdOracle].value(cx),
                    "unfinished oracle"
                );
                assert_eq!(editor.snapshot.revision, "saved-revision");
                assert!(!editor.stale);
                assert!(editor.only_pricing_differs(editor.draft.as_ref().unwrap(), cx));
                let FieldInput::Single(input) = &editor.fields[&ChainField::BlockRange] else {
                    unreachable!()
                };
                input.update(cx, |input, cx| input.set_value("123", window, cx));
                assert!(!editor.only_pricing_differs(editor.draft.as_ref().unwrap(), cx));
            });
            window.set_rem_size(px(22.0));
            window.draw(cx).clear(cx);
        });
        let save = cx
            .debug_bounds("chain-save")
            .expect("save stays rendered at enlarged scale");
        assert!(save.bottom() <= px(480.0));
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.retire(cx);
                assert!(editor.pricing_status.is_empty());
                assert!(!editor.is_editing());
                window.remove_window();
            });
        });
    }
}
