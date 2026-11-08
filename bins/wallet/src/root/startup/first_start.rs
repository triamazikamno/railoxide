//! First-start setup: the responsibility notice, the connection and the chains, shown before
//! network startup on a data folder without a completed setup record.

use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement, SharedString, Styled,
    Subscription, Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, IconName, Sizable, WindowExt,
    alert::Alert,
    button::ButtonVariants,
    checkbox::Checkbox,
    input::{InputEvent, InputState},
    popover::Popover,
    radio::{Radio, RadioGroup},
    scroll::ScrollableElement,
    switch::Switch,
    tag::Tag,
};
use railgun_ui::{DEFAULT_CHAINS, built_in_chain_ids, chain_icon_asset_path, chain_name};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text, app_text};
use ui::dialog::WalletDialogExt as _;
use ui::hint::hint_card;
use ui::theme;
use wallet_ops::settings::{
    NetworkModeSetting, PROXY_URL_REQUIRED, PoiReadSourceSetting, WalletFirstStart,
    load_first_start, load_wallet_settings, save_first_start,
};
use wallet_ops::vault::DesktopVaultStore;

use super::WalletStartupRoot;
use crate::root::private_action::render_private_action_info_icon_with_body;
use crate::root::private_swap::ui_fixture_first_start_old_notice;
use crate::root::rgb_with_alpha;
use crate::root::settings::{
    CHAINS_PAGE_INDEX, PROXY_SOCKS5_DNS_WARNING, SyncedStringFieldState, WalletSettingsEditor,
    format_auto_lock_timeout, non_empty_setting, proxy_url_resolves_dns_locally,
    settings_dialog_dimensions, settings_text_input, settings_warning_banner,
};
use crate::root::shell::render_wallet_hero_screen;

/// Bump with any change to `NOTICE_POINTS`, so every data folder shows the notice again.
const FIRST_START_NOTICE_VERSION: u32 = 1;

const NOTICE_POINTS: &[&str] = &[
    "Keep your recovery phrase and backups safe and private.",
    "If you lose access to your wallet and have no recovery phrase or backup, no one can recover your funds, including the developers.",
    "Keep this device secure.",
    "This wallet is experimental software, provided under the MIT license without warranty of any kind.",
    "Review the source code and verify the build before trusting this wallet with funds.",
];

const RAILGUN_INDEXER_URL: &str = "https://github.com/triamazikamno/railgun-indexer";

const NETWORK_MODES: [NetworkModeSetting; 3] = [
    NetworkModeSetting::Tor,
    NetworkModeSetting::Proxy,
    NetworkModeSetting::Direct,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstStartStep {
    Notice,
    Connection,
    Chains,
}

pub(super) struct FirstStartSetup {
    step: FirstStartStep,
    acknowledged: bool,
    /// Setup finished earlier under an older notice, so only the notice is shown.
    notice_only: bool,
    /// The data folder already has a vault, so setup tells the user their data is intact.
    existing_vault: bool,
    /// Owned by setup alone; its pre-start flag keeps Save from starting networking.
    editor: Entity<WalletSettingsEditor>,
    /// Created on first render, which has the window an input needs.
    proxy: Option<SyncedStringFieldState>,
    error: Option<SharedString>,
    _editor_observer: Subscription,
}

impl WalletStartupRoot {
    /// Reads the setup record before anything can connect. Returns whether network startup may
    /// begin. Otherwise setup is shown, or a startup error when the record or settings can't be
    /// read.
    pub(super) fn first_start_admits_startup(
        &mut self,
        store: &Arc<DesktopVaultStore>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let db = store.db();
        let record = match load_first_start(db.as_ref()) {
            Ok(record) => record,
            Err(error) => {
                self.fail_startup(format!("Failed to read first-start setup: {error}"), cx);
                return false;
            }
        };
        let accepted_notice_version = if ui_fixture_first_start_old_notice() {
            0
        } else {
            record.accepted_notice_version
        };
        if record.setup_complete && accepted_notice_version >= FIRST_START_NOTICE_VERSION {
            self.setup = None;
            return true;
        }
        let settings = match load_wallet_settings(db.as_ref()) {
            Ok(settings) => settings,
            Err(error) => {
                self.fail_startup(format!("Failed to load wallet settings: {error}"), cx);
                return false;
            }
        };
        // Only a hint for the notice, so a failed read just leaves it out.
        let existing_vault = store.vault_exists().unwrap_or(false);
        let store = Arc::clone(store);
        let db_path = self.options.db_path.clone();
        let runtime = self.runtime.clone();
        let maintenance_controller = self.maintenance_controller.clone();
        let startup_root = cx.weak_entity();
        let editor = cx.new(|cx| {
            let mut editor = WalletSettingsEditor::new(
                store,
                &db_path,
                runtime,
                settings,
                maintenance_controller,
                Some(startup_root),
                None,
                window,
                cx,
            );
            editor.set_pre_start(cx);
            editor
        });
        // Saves from the Settings dialog re-render the step from the shared draft.
        let editor_observer = cx.observe(&editor, |_root, _editor, cx| cx.notify());
        self.error = None;
        self.setup = Some(FirstStartSetup {
            step: FirstStartStep::Notice,
            acknowledged: false,
            notice_only: record.setup_complete,
            existing_vault,
            editor,
            proxy: None,
            error: None,
            _editor_observer: editor_observer,
        });
        cx.notify();
        false
    }

    /// Opens the setup's own editor in the Settings dialog, optionally on `page`.
    pub(super) fn open_first_start_settings(
        &self,
        page: Option<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        let editor = setup.editor.clone();
        if let Some(page) = page {
            editor.update(cx, |editor, _cx| editor.open_on_page(page));
        }
        window.close_all_dialogs(cx);
        let (dialog_width, content_height, dialog_max_height) = settings_dialog_dimensions(window);
        window.open_wallet_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .margin_top(px(16.0))
                .title(app_strong_text("Settings"))
                .on_ok(|_, _, _| false)
                .child(div().h(content_height).min_h(px(0.0)).child(editor.clone()))
        });
    }

    /// Keeps the proxy URL input following the draft by value, so edits made in Settings show
    /// here whether saved or not, without overwriting what is typed here.
    pub(super) fn sync_first_start_proxy_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        let (value, revision) = {
            let editor = setup.editor.read(cx);
            (
                SharedString::from(editor.draft.network.proxy_url.clone().unwrap_or_default()),
                editor.field_sync_revision,
            )
        };
        if setup.proxy.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("socks5h://127.0.0.1:9050")
                    .default_value(value)
            });
            let subscription = cx.subscribe(&input, |root, input, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let Some(editor) = root.setup.as_ref().map(|setup| setup.editor.clone()) else {
                    return;
                };
                let value = input.read(cx).value().to_string();
                editor.update(cx, |editor, cx| editor.set_draft_proxy_url(&value, cx));
            });
            setup.proxy = Some(SyncedStringFieldState {
                input,
                synced_revision: revision,
                _subscription: subscription,
            });
            return;
        }
        let Some(state) = setup.proxy.as_ref() else {
            return;
        };
        let text = state.input.read(cx).value();
        // `set_value` resets selection and undo even for identical text, and the draft keeps
        // only the trimmed form of what was typed here.
        if text.as_ref() == value.as_ref()
            || non_empty_setting(&text) == setup.editor.read(cx).draft.network.proxy_url
        {
            return;
        }
        state
            .input
            .update(cx, |input, cx| input.set_value(value, window, cx));
    }

    fn update_first_start(
        &mut self,
        cx: &mut Context<'_, Self>,
        update: impl FnOnce(&mut FirstStartSetup),
    ) {
        if let Some(setup) = self.setup.as_mut() {
            update(setup);
            cx.notify();
        }
    }

    fn first_start_continue(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        match setup.step {
            FirstStartStep::Notice if !setup.acknowledged => {}
            FirstStartStep::Notice if !setup.notice_only => {
                setup.step = FirstStartStep::Connection;
                cx.notify();
            }
            FirstStartStep::Connection => {
                if setup.editor.read(cx).validation_error.is_none() {
                    setup.step = FirstStartStep::Chains;
                    cx.notify();
                }
            }
            FirstStartStep::Notice | FirstStartStep::Chains => self.finish_first_start(window, cx),
        }
    }

    /// Saves the settings, then the setup record, then starts network startup. A failed write
    /// keeps the step with its error and starts nothing.
    fn finish_first_start(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        // The notice alone changes no settings, so only the record is written then.
        if !setup.notice_only {
            let editor = setup.editor.clone();
            if !editor.update(cx, WalletSettingsEditor::persist_draft) {
                let editor = editor.read(cx);
                setup.error = editor
                    .validation_error
                    .clone()
                    .or_else(|| editor.status.clone())
                    .map(|error| SharedString::from(error.to_string()));
                cx.notify();
                return;
            }
        }
        let record = WalletFirstStart {
            setup_complete: true,
            accepted_notice_version: FIRST_START_NOTICE_VERSION,
            ..WalletFirstStart::default()
        };
        if let Err(error) = save_first_start(store.db().as_ref(), &record) {
            setup.error = Some(format!("Failed to save setup: {error}").into());
            cx.notify();
            return;
        }
        self.setup = None;
        self.retry_startup_with_network_context(None, window, cx);
    }

    pub(super) fn render_first_start(
        &self,
        window: &Window,
        cx: &Context<'_, Self>,
    ) -> gpui::AnyElement {
        let Some(setup) = self.setup.as_ref() else {
            return div().into_any_element();
        };
        let root = cx.entity();
        let editor = setup.editor.read(cx);
        let invalid = editor.validation_error.is_some();
        let (step_label, title, body, can_continue) = match setup.step {
            FirstStartStep::Notice => (
                (!setup.notice_only).then_some("Step 1 of 3"),
                if setup.notice_only {
                    "Security and risks have changed"
                } else {
                    "Security and risks"
                },
                Self::render_first_start_notice(
                    root.clone(),
                    setup.acknowledged,
                    setup.existing_vault && !setup.notice_only,
                ),
                setup.acknowledged,
            ),
            FirstStartStep::Connection => (
                Some("Step 2 of 3"),
                "Connection",
                Self::render_first_start_connection(root.clone(), setup, cx),
                !invalid,
            ),
            FirstStartStep::Chains => (
                Some("Step 3 of 3"),
                "Chains",
                Self::render_first_start_chains(root.clone(), setup, cx),
                !invalid,
            ),
        };
        let back_root = root.clone();
        let continue_root = root;
        let card = div()
            .w_full()
            .p(px(28.0))
            .flex()
            .flex_col()
            .gap_5()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER_STRONG))
            .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.86))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when_some(step_label, |header, label| {
                        header.child(app_muted_text(label))
                    })
                    .child(
                        app_strong_text(title)
                            .text_size(px(22.0))
                            .line_height(px(28.0)),
                    ),
            )
            .child(body)
            .when_some(setup.error.clone(), |card, error| {
                card.child(
                    app_muted_text(error)
                        .text_color(rgb(theme::DANGER))
                        .whitespace_normal(),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .when(setup.step != FirstStartStep::Notice, |footer| {
                        footer.child(app_button("first-start-back", "Back").on_click(
                            move |_event, _window, cx| {
                                back_root.update(cx, |root, cx| {
                                    root.update_first_start(cx, |setup| {
                                        setup.error = None;
                                        setup.step = match setup.step {
                                            FirstStartStep::Chains => FirstStartStep::Connection,
                                            _ => FirstStartStep::Notice,
                                        };
                                    });
                                });
                            },
                        ))
                    })
                    .child(
                        app_button("first-start-continue", "Continue")
                            .primary()
                            .disabled(!can_continue)
                            .on_click(move |_event, window, cx| {
                                continue_root.update(cx, |root, cx| {
                                    root.first_start_continue(window, cx);
                                });
                            }),
                    ),
            )
            .into_any_element();
        render_wallet_hero_screen(window, card, None).into_any_element()
    }

    fn render_first_start_notice(
        root: Entity<Self>,
        acknowledged: bool,
        existing_vault: bool,
    ) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .when(existing_vault, |notice| {
                notice.child(
                    Alert::info(
                        "first-start-existing-vault",
                        "This version adds a one-time review of your settings. Your vault and wallets are unchanged, and you unlock with the same password afterwards.",
                    )
                    .title("Your wallet data is intact")
                    .small(),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(NOTICE_POINTS.iter().map(|point| {
                        div()
                            .flex()
                            .gap_2()
                            .child(app_muted_text("•"))
                            .child(app_text(*point).flex_1().min_w(px(0.0)).whitespace_normal())
                    })),
            )
            .child(
                Checkbox::new("first-start-acknowledge")
                    .label("I understand the risks")
                    .checked(acknowledged)
                    .on_click(move |checked, _window, cx| {
                        let checked = *checked;
                        root.update(cx, |root, cx| {
                            root.update_first_start(cx, |setup| setup.acknowledged = checked);
                        });
                    }),
            )
    }

    fn render_first_start_connection(
        root: Entity<Self>,
        setup: &FirstStartSetup,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let editor = setup.editor.read(cx);
        let draft = &editor.draft;
        let mode = draft.network.mode;
        let mode_editor = setup.editor.clone();
        let note = match mode {
            NetworkModeSetting::Tor => "Wallet traffic goes through the built-in Tor client.",
            NetworkModeSetting::Proxy => {
                "Broadcasters are disabled in proxy mode because their connections may bypass the proxy and expose your IP address."
            }
            NetworkModeSetting::Direct => "RPCs and peers see your IP address.",
        };
        let (poi, official_publisher) = match draft.poi.read_source {
            PoiReadSourceSetting::IndexedArtifacts if editor.poi_custom_publisher_selected() => {
                ("Indexed artifacts, custom publisher", false)
            }
            PoiReadSourceSetting::IndexedArtifacts => {
                ("Indexed artifacts, official publisher", true)
            }
            PoiReadSourceSetting::PoiProxy => ("PPOI proxy", false),
        };
        let broadcasters = if mode == NetworkModeSetting::Proxy {
            "Off in proxy mode"
        } else {
            "Over Waku once the wallet is unlocked"
        };
        let auto_lock = draft.runtime.auto_lock_timeout_secs.map_or_else(
            || "Off".to_owned(),
            |timeout| format!("After {}", format_auto_lock_timeout(timeout)),
        );
        let summary_row =
            |label: &'static str, value: SharedString, trailing: Option<gpui::AnyElement>| {
                div()
                    .flex()
                    .gap_3()
                    .child(app_muted_text(label).w(px(120.0)).flex_none())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(app_text(value).min_w(px(0.0)).whitespace_normal())
                            .children(trailing),
                    )
            };
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        RadioGroup::vertical("first-start-network-mode")
                            .child(
                                Radio::new("first-start-tor").label("Built-in Tor (recommended)"),
                            )
                            .child(Radio::new("first-start-proxy").label("Proxy"))
                            .child(Radio::new("first-start-direct").label("Direct"))
                            .selected_index(NETWORK_MODES.iter().position(|item| *item == mode))
                            .on_change(move |index, _window, cx| {
                                let Some(mode) = NETWORK_MODES.get(*index).copied() else {
                                    return;
                                };
                                mode_editor.update(cx, |editor, cx| {
                                    editor.set_draft_network_mode(mode, cx);
                                });
                            }),
                    )
                    .when(mode == NetworkModeSetting::Proxy, |section| {
                        section.when_some(setup.proxy.as_ref(), |section, proxy| {
                            section.child(settings_text_input(&proxy.input))
                        })
                    })
                    .when(proxy_url_resolves_dns_locally(draft), |section| {
                        section.child(settings_warning_banner(
                            "first-start-proxy-dns-warning",
                            PROXY_SOCKS5_DNS_WARNING,
                        ))
                    })
                    .child(app_muted_text(note).whitespace_normal())
                    .when_some(editor.validation_error.clone(), |section, error| {
                        // An empty proxy field isn't a mistake yet, so it gets a hint, not red.
                        section.child(if error.as_ref() == PROXY_URL_REQUIRED {
                            app_muted_text("Enter your proxy address to continue.")
                                .whitespace_normal()
                        } else {
                            app_muted_text(error.to_string())
                                .text_color(rgb(theme::DANGER))
                                .whitespace_normal()
                        })
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(app_strong_text("Other defaults"))
                    .child(summary_row(
                        "PPOI data",
                        poi.into(),
                        official_publisher
                            .then(|| render_official_publisher_info().into_any_element()),
                    ))
                    .child(summary_row("Broadcasters", broadcasters.into(), None))
                    .child(summary_row("Auto-lock", auto_lock.into(), None))
                    .child(
                        div().pt(px(4.0)).flex().child(
                            app_button("first-start-edit-settings", "Edit in Settings…")
                                .outline()
                                .on_click(move |_event, window, cx| {
                                    root.update(cx, |root, cx| {
                                        root.open_first_start_settings(None, window, cx);
                                    });
                                }),
                        ),
                    ),
            )
    }

    fn render_first_start_chains(
        root: Entity<Self>,
        setup: &FirstStartSetup,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let editor = setup.editor.read(cx);
        let enabled = editor.draft.chains.enabled_chain_ids();
        let railgun_only_disabled = non_railgun_chains_to_disable(&enabled).is_empty();
        let railgun_only_editor = setup.editor.clone();
        let validation_error = editor.validation_error.clone();
        // Settings requires one enabled chain, so the only one left can't be switched off.
        let locked = match enabled.as_slice() {
            [only] if railgun_ui::is_built_in_chain(*only) => Some(*only),
            _ => None,
        };
        let rows = built_in_chain_ids().map(|chain_id| {
            let checked = enabled.contains(&chain_id);
            let name = chain_name(chain_id).unwrap_or("Unknown chain");
            let editor = setup.editor.clone();
            div()
                .flex()
                .items_center()
                .gap_3()
                .py(px(6.0))
                .child(
                    div()
                        .size(px(20.0))
                        .flex_none()
                        .when_some(chain_icon_asset_path(chain_id), |slot, path| {
                            slot.child(img(path).size(px(20.0)))
                        }),
                )
                .child(app_text(name).flex_1().min_w(px(0.0)))
                .when(DEFAULT_CHAINS.contains(&chain_id), |row| {
                    row.child(Tag::secondary().small().rounded_full().child("Railgun"))
                })
                .child(
                    Switch::new(SharedString::from(format!("first-start-chain-{chain_id}")))
                        .accessibility_label(format!("Enable {name}"))
                        .checked(checked)
                        .disabled(locked == Some(chain_id))
                        .on_change(move |enabled, _window, cx| {
                            let enabled = *enabled;
                            editor.update(cx, |editor, cx| {
                                editor.set_draft_chain_enabled(chain_id, enabled, cx);
                            });
                        }),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(app_strong_text("Built-in chains"))
                    .child(render_private_action_info_icon_with_body(
                        "first-start-chains-info".into(),
                        "Startup RPC checks",
                        || {
                            app_muted_text(
                                "The wallet checks the RPC of each enabled Railgun chain at startup. It contacts other chains only when you select them.",
                            )
                            .whitespace_normal()
                            .into_any_element()
                        },
                    ))
                    .child(div().flex_1())
                    .child(
                        app_button("first-start-railgun-chains-only", "Railgun chains only")
                            .outline()
                            .small()
                            .disabled(railgun_only_disabled)
                            .on_click(move |_event, _window, cx| {
                                railgun_only_editor.update(cx, disable_non_railgun_chains);
                            }),
                    )
                    .child(
                        app_button("first-start-custom-rpcs", "Custom RPCs…")
                            .outline()
                            .small()
                            .on_click(move |_event, window, cx| {
                                root.update(cx, |root, cx| {
                                    root.open_first_start_settings(
                                        Some(CHAINS_PAGE_INDEX),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    ),
            )
            // The list scrolls on its own so the step's header and footer stay in view.
            .child(
                div()
                    .max_h(px(360.0))
                    .flex()
                    .flex_col()
                    .pr(px(12.0))
                    .overflow_y_scrollbar()
                    .children(rows),
            )
            .when(locked.is_some(), |list| {
                list.child(
                    app_muted_text("At least one chain must stay enabled.").whitespace_normal(),
                )
            })
            .when_some(validation_error, |list, error| {
                list.child(
                    app_muted_text(error.to_string())
                        .text_color(rgb(theme::DANGER))
                        .whitespace_normal(),
                )
            })
    }
}

/// The enabled built-in chains outside Railgun that "Railgun chains only" switches off. Empty
/// when none is enabled or no Railgun chain is, since Settings requires one enabled chain.
fn non_railgun_chains_to_disable(enabled: &[u64]) -> Vec<u64> {
    if !enabled
        .iter()
        .any(|chain_id| DEFAULT_CHAINS.contains(chain_id))
    {
        return Vec::new();
    }
    enabled
        .iter()
        .copied()
        .filter(|chain_id| {
            railgun_ui::is_built_in_chain(*chain_id) && !DEFAULT_CHAINS.contains(chain_id)
        })
        .collect()
}

fn disable_non_railgun_chains(
    editor: &mut WalletSettingsEditor,
    cx: &mut Context<'_, WalletSettingsEditor>,
) {
    for chain_id in non_railgun_chains_to_disable(&editor.draft.chains.enabled_chain_ids()) {
        editor.set_draft_chain_enabled(chain_id, false, cx);
    }
}

/// An info button whose popover explains the official PPOI publisher and links its indexer.
fn render_official_publisher_info() -> impl IntoElement {
    Popover::new("first-start-official-publisher-popover")
        .trigger(
            app_button_base("first-start-official-publisher-info")
                .ghost()
                .xsmall()
                .icon(IconName::Info)
                .accessibility_label("About the official publisher"),
        )
        .content(|_, window, _| {
            hint_card("Official publisher", theme::INFO, window)
                .child(div().whitespace_normal().child(
                    "The official publisher signs the indexed PPOI artifacts the wallet reads. You can run your own indexer and use it as a custom publisher in Settings > Data sources.",
                ))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .whitespace_normal()
                                .font_family(theme::APP_MONO_FONT_FAMILY)
                                .child(RAILGUN_INDEXER_URL),
                        )
                        .child(
                            clipboard_with_toast(
                                "first-start-indexer-url-copy",
                                RAILGUN_INDEXER_URL,
                            )
                            .tooltip("Copy link"),
                        ),
                )
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gpui::{AnyWindowHandle, TestAppContext};
    use wallet_ops::settings::{WALLET_SETTINGS_KEY, WalletSettings, save_wallet_settings};

    use super::*;
    use crate::root::shell::WalletAppOptions;

    /// Never driven, so a spawned startup task stays pending and contacts nothing.
    fn idle_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    struct Harness {
        db_path: PathBuf,
        runtime: tokio::runtime::Runtime,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.db_path);
        }
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            Self {
                db_path: std::env::temp_dir().join(format!(
                    "railoxide-first-start-{name}-{}-{nanos}",
                    std::process::id()
                )),
                runtime: idle_runtime(),
            }
        }

        fn open(&self, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<WalletStartupRoot>) {
            let (event_tx, event_rx) = broadcaster_monitor::event_channel(16);
            let options = WalletAppOptions {
                db_path: self.db_path.clone(),
            };
            let runtime = self.runtime.handle().clone();
            let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
            let root_slot = slot.clone();
            let handle = cx.add_window(|window, cx| {
                let root = cx.new(|cx| {
                    WalletStartupRoot::new(
                        options,
                        runtime,
                        broadcaster_monitor::shared(),
                        event_tx,
                        event_rx,
                        &[],
                        ui::logs::LogStore::new(1),
                        window,
                        cx,
                    )
                });
                *root_slot.borrow_mut() = Some(root.clone());
                gpui_component::Root::new(root, window, cx)
            });
            let root = slot.borrow_mut().take().unwrap();
            (handle.into(), root)
        }

        /// Startup builds its network context in the task it spawns on this runtime.
        fn startup_tasks(&self) -> usize {
            self.runtime.metrics().num_alive_tasks()
        }

        /// Opens the database while no root holds it; the store keeps it locked.
        fn store(&self) -> DesktopVaultStore {
            DesktopVaultStore::open(self.db_path.clone()).unwrap()
        }
    }

    /// The root's own store, since a second open of a locked database fails.
    fn root_store(root: &Entity<WalletStartupRoot>, cx: &TestAppContext) -> Arc<DesktopVaultStore> {
        root.read_with(cx, |root, _| root.vault_store.clone().unwrap())
    }

    fn step(root: &Entity<WalletStartupRoot>, cx: &TestAppContext) -> Option<FirstStartStep> {
        root.read_with(cx, |root, _| root.setup.as_ref().map(|setup| setup.step))
    }

    fn in_window(
        handle: AnyWindowHandle,
        root: &Entity<WalletStartupRoot>,
        cx: &mut TestAppContext,
        update: impl FnOnce(&mut WalletStartupRoot, &mut Window, &mut Context<'_, WalletStartupRoot>),
    ) {
        cx.update_window(handle, |_, window, cx| {
            root.update(cx, |root, cx| update(root, window, cx));
        })
        .unwrap();
    }

    fn acknowledge_and_continue(
        handle: AnyWindowHandle,
        root: &Entity<WalletStartupRoot>,
        cx: &mut TestAppContext,
    ) {
        in_window(handle, root, cx, |root, window, cx| {
            root.first_start_continue(window, cx);
            assert_eq!(
                root.setup.as_ref().map(|setup| setup.step),
                Some(FirstStartStep::Notice),
                "Continue needs I understand the risks"
            );
            root.update_first_start(cx, |setup| setup.acknowledged = true);
            root.first_start_continue(window, cx);
        });
    }

    #[gpui::test]
    fn setup_holds_network_startup_until_finished_and_is_skipped_afterwards(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let mut harness = Harness::new("fresh");
        let (handle, root) = harness.open(cx);
        assert_eq!(step(&root, cx), Some(FirstStartStep::Notice));
        root.read_with(cx, |root, _| {
            assert!(root.error.is_none() && root.wallet_root.is_none());
        });
        assert_eq!(harness.startup_tasks(), 0);

        // The shortcut opens the setup's own pre-start editor, not an ordinary one.
        cx.update_window(handle, |_, window, cx| {
            root.update(cx, |root, cx| root.open_settings_from_shortcut(window, cx));
            assert!(window.has_active_dialog(cx));
            window.close_all_dialogs(cx);
        })
        .unwrap();
        let editor = root.read_with(cx, |root, _| root.setup.as_ref().unwrap().editor.clone());
        assert!(editor.read_with(cx, |editor, _| editor.pre_start));

        acknowledge_and_continue(handle, &root, cx);
        editor.update(cx, |editor, cx| {
            editor.set_draft_network_mode(NetworkModeSetting::Direct, cx);
        });
        in_window(handle, &root, cx, |root, window, cx| {
            root.first_start_continue(window, cx);
        });
        assert_eq!(step(&root, cx), Some(FirstStartStep::Chains));
        assert_eq!(harness.startup_tasks(), 0);

        in_window(handle, &root, cx, |root, window, cx| {
            root.first_start_continue(window, cx);
        });
        assert_eq!(step(&root, cx), None);
        assert_eq!(
            harness.startup_tasks(),
            1,
            "finishing setup starts networking"
        );
        let store = root_store(&root, cx);
        assert_eq!(
            load_wallet_settings(store.db().as_ref())
                .unwrap()
                .network
                .mode,
            NetworkModeSetting::Direct
        );
        assert_eq!(
            load_first_start(store.db().as_ref()).unwrap(),
            WalletFirstStart {
                setup_complete: true,
                accepted_notice_version: FIRST_START_NOTICE_VERSION,
                ..WalletFirstStart::default()
            }
        );

        // Release every holder of the database lock before the next launch opens it.
        drop((store, editor));
        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
        drop(root);
        harness.runtime = idle_runtime();
        cx.run_until_parked();
        let (_handle, next) = harness.open(cx);
        assert_eq!(step(&next, cx), None);
        assert!(next.read_with(cx, |root, _| root.error.is_none()));
        assert_eq!(
            harness.startup_tasks(),
            1,
            "the next launch starts as before"
        );
    }

    #[gpui::test]
    fn setup_proxy_input_follows_settings_edits(cx: &mut TestAppContext) {
        fn proxy_text(root: &Entity<WalletStartupRoot>, cx: &TestAppContext) -> String {
            root.read_with(cx, |root, cx| {
                let proxy = root.setup.as_ref().unwrap().proxy.as_ref().unwrap();
                proxy.input.read(cx).value().to_string()
            })
        }

        cx.update(gpui_component::init);
        let harness = Harness::new("proxy");
        let (handle, root) = harness.open(cx);
        acknowledge_and_continue(handle, &root, cx);
        let editor = root.read_with(cx, |root, _| root.setup.as_ref().unwrap().editor.clone());
        editor.update(cx, |editor, cx| {
            editor.set_draft_network_mode(NetworkModeSetting::Proxy, cx);
        });

        // Typed here: the draft trims it, and the input keeps the text as typed.
        let typed = "socks5h://127.0.0.1:9050 ";
        in_window(handle, &root, cx, |root, window, cx| {
            root.sync_first_start_proxy_input(window, cx);
            let setup = root.setup.as_ref().unwrap();
            let input = setup.proxy.as_ref().unwrap().input.clone();
            let editor = setup.editor.clone();
            input.update(cx, |input, cx| input.set_value(typed, window, cx));
            editor.update(cx, |editor, cx| editor.set_draft_proxy_url(typed, cx));
            root.sync_first_start_proxy_input(window, cx);
        });
        assert_eq!(proxy_text(&root, cx), typed);

        // Typed in Settings, which changes the shared draft without touching setup's input.
        let edited = "socks5h://127.0.0.1:9150";
        editor.update(cx, |editor, cx| editor.set_draft_proxy_url(edited, cx));
        in_window(handle, &root, cx, |root, window, cx| {
            root.sync_first_start_proxy_input(window, cx);
        });
        assert_eq!(proxy_text(&root, cx), edited);

        for _ in 0..2 {
            in_window(handle, &root, cx, |root, window, cx| {
                root.first_start_continue(window, cx);
            });
        }
        assert_eq!(step(&root, cx), None);
        let settings = load_wallet_settings(root_store(&root, cx).db().as_ref()).unwrap();
        assert_eq!(settings.network.proxy_url.as_deref(), Some(edited));
    }

    #[gpui::test]
    fn railgun_chains_only_keeps_just_the_enabled_railgun_chains(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let harness = Harness::new("railgun-only");
        let (handle, root) = harness.open(cx);
        acknowledge_and_continue(handle, &root, cx);
        in_window(handle, &root, cx, |root, window, cx| {
            root.first_start_continue(window, cx);
        });
        assert_eq!(step(&root, cx), Some(FirstStartStep::Chains));

        let editor = root.read_with(cx, |root, _| root.setup.as_ref().unwrap().editor.clone());
        let enabled = |cx: &TestAppContext| {
            editor.read_with(cx, |editor, _| editor.draft.chains.enabled_chain_ids())
        };
        let railgun: Vec<u64> = enabled(cx)
            .into_iter()
            .filter(|chain_id| DEFAULT_CHAINS.contains(chain_id))
            .collect();
        assert_ne!(railgun, Vec::<u64>::new());
        let other = built_in_chain_ids()
            .find(|chain_id| !DEFAULT_CHAINS.contains(chain_id))
            .unwrap();
        editor.update(cx, |editor, cx| {
            editor.set_draft_chain_enabled(other, true, cx);
        });
        assert!(enabled(cx).contains(&other));
        // Without a Railgun chain the action would leave none enabled, so it does nothing.
        assert_eq!(non_railgun_chains_to_disable(&[other]), Vec::<u64>::new());

        editor.update(cx, disable_non_railgun_chains);
        assert_eq!(enabled(cx), railgun);
        assert_eq!(non_railgun_chains_to_disable(&railgun), Vec::<u64>::new());
    }

    #[gpui::test]
    fn retry_after_database_open_failure_enters_setup(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let harness = Harness::new("retry");
        // A file where the data folder belongs keeps the database from opening.
        std::fs::write(&harness.db_path, b"").unwrap();
        let (handle, root) = harness.open(cx);
        root.read_with(cx, |root, _| {
            assert!(root.error.is_some() && root.setup.is_none());
        });

        std::fs::remove_file(&harness.db_path).unwrap();
        in_window(handle, &root, cx, |root, window, cx| {
            root.retry_startup(window, cx);
        });
        assert_eq!(step(&root, cx), Some(FirstStartStep::Notice));
        assert!(root.read_with(cx, |root, _| root.error.is_none()));
        assert_eq!(harness.startup_tasks(), 0);
    }

    #[gpui::test]
    fn changed_notice_shows_only_the_notice_and_saves_only_its_version(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let harness = Harness::new("notice");
        let store = harness.store();
        let mut settings = WalletSettings::default();
        settings.network.mode = NetworkModeSetting::Direct;
        save_wallet_settings(store.db().as_ref(), &settings).unwrap();
        save_first_start(
            store.db().as_ref(),
            &WalletFirstStart {
                setup_complete: true,
                accepted_notice_version: 0,
                ..WalletFirstStart::default()
            },
        )
        .unwrap();
        let settings_record = store
            .db()
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .unwrap();
        drop(store);

        let (handle, root) = harness.open(cx);
        assert!(root.read_with(cx, |root, _| root.setup.as_ref().unwrap().notice_only));
        // Continue on the notice finishes, without the connection and chains steps.
        acknowledge_and_continue(handle, &root, cx);
        assert_eq!(step(&root, cx), None);
        assert_eq!(harness.startup_tasks(), 1);
        let store = root_store(&root, cx);
        assert_eq!(
            store
                .db()
                .get_app_settings_record(WALLET_SETTINGS_KEY)
                .unwrap(),
            settings_record
        );
        assert_eq!(
            load_first_start(store.db().as_ref())
                .unwrap()
                .accepted_notice_version,
            FIRST_START_NOTICE_VERSION
        );
    }
}
