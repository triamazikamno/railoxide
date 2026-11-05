//! The on-demand update check: a disclosure, one request to GitHub over the wallet's network
//! mode once the user confirms, and the release timeline split at the installed build. Nothing
//! is kept after the dialog closes.

use std::collections::BTreeSet;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, FocusHandle, IntoElement,
    ParentElement as _, SharedString, Styled as _, WeakFocusHandle, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    collapsible::Collapsible,
    dialog::DialogFooter,
    spinner::Spinner,
    text::TextView,
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_button_label, app_muted_shimmer_text, app_muted_text,
    app_strong_text, app_text,
};
use ui::dialog::WalletDialogExt as _;
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::WalletNetworkMode;
use wallet_ops::release_check::{
    GithubRelease, InstalledMarker, ReleaseCheckError, ReleaseTimeline, check_releases,
};

use super::markdown_sanitize::{max_backtick_run, sanitize_render_source};
use super::{WalletRoot, dialog_max_height, rgb_with_alpha, secondary_dialog_content_width};

const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_FOR_UPDATES: &str = "Check for updates";
const DIALOG_WIDTH: f32 = 640.0;

/// Opens the update check dialog from a build label's button.
pub(super) type UpdateCheckTrigger = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Default)]
pub(super) struct UpdateCheckUi {
    /// `None` while the dialog is closed.
    dialog: Option<UpdateCheckDialog>,
    /// Kept across close and reopen, so a late result of an earlier check never matches.
    generation: u64,
}

impl UpdateCheckUi {
    /// Dropping a tokio `JoinHandle` doesn't cancel its task, so a running check is aborted
    /// explicitly, which drops its operation client and stops its Tor bridge.
    fn abort_running(&self) {
        if let Some(UpdateCheckDialog {
            state: UpdateCheckState::Checking { abort, .. },
            ..
        }) = &self.dialog
        {
            abort.abort();
        }
    }
}

struct UpdateCheckDialog {
    /// The open dialog's builder entity, whose release closes this state.
    builder: EntityId,
    /// The dialog's own focus, taken back when the focused footer button leaves the footer.
    focus: Option<WeakFocusHandle>,
    state: UpdateCheckState,
    /// Timeline entries whose notes are shown.
    expanded: BTreeSet<usize>,
}

impl UpdateCheckDialog {
    fn show(&mut self, result: Result<ReleaseTimeline, ReleaseCheckError>) {
        match result {
            Ok(timeline) => {
                self.expanded = initial_expansion(&timeline);
                self.state = UpdateCheckState::Done(timeline);
            }
            Err(error) => {
                self.expanded.clear();
                self.state = UpdateCheckState::Failed(error);
            }
        }
    }
}

enum UpdateCheckState {
    Disclosure,
    Checking {
        generation: u64,
        abort: tokio::task::AbortHandle,
    },
    Done(ReleaseTimeline),
    Failed(ReleaseCheckError),
}

/// Releases above the installed marker when any are newer, otherwise the installed release.
fn initial_expansion(timeline: &ReleaseTimeline) -> BTreeSet<usize> {
    match timeline.installed {
        _ if timeline.newer_count > 0 => (0..timeline.newer_count).collect(),
        InstalledMarker::At(index) => BTreeSet::from([index]),
        InstalledMarker::Before(_) | InstalledMarker::Unknown => BTreeSet::new(),
    }
}

/// The icon button beside a build label that opens the update check dialog.
pub(super) fn update_check_button(id: &'static str, trigger: UpdateCheckTrigger) -> Button {
    Button::new(id)
        .icon(IconName::RefreshCw)
        .ghost()
        .xsmall()
        .tooltip(CHECK_FOR_UPDATES)
        .accessibility_label(CHECK_FOR_UPDATES)
        .on_click(move |_event, window, cx| trigger(window, cx))
}

impl WalletRoot {
    pub(super) fn update_check_trigger(root: Entity<Self>) -> UpdateCheckTrigger {
        Rc::new(move |window, cx| {
            root.update(cx, |root, cx| root.open_update_check_dialog(window, cx));
        })
    }

    /// Opens the dialog on its disclosure. Nothing is sent until the user confirms.
    pub(super) fn open_update_check_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.update_check.abort_running();
        // Closing the dialog by any path drops its builder and this entity with it, so the
        // check is cancelled even when on_close doesn't run.
        let builder_alive = cx.new(|_| ());
        let builder = builder_alive.entity_id();
        cx.observe_release(&builder_alive, move |root, (), cx| {
            if root
                .update_check
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.builder == builder)
            {
                root.close_update_check(cx);
            }
        })
        .detach();
        self.update_check.dialog = Some(UpdateCheckDialog {
            builder,
            focus: None,
            state: UpdateCheckState::Disclosure,
            expanded: BTreeSet::new(),
        });
        let root = cx.entity();
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            // Only the open dialog's builder owns the entity. Programmatic closure skips on_close.
            let _ = &builder_alive;
            let width = (window.viewport_size().width * 0.92).min(px(DIALOG_WIDTH));
            let close_root = root.clone();
            let ok_root = root.clone();
            let content = root.read(cx);
            let body = content.render_update_check_body(&root);
            let footer = content.render_update_check_footer(&root);
            dialog
                .w(width)
                .max_h(dialog_max_height(window))
                .on_ok(move |_event, window, cx| {
                    ok_root.update(cx, |root, cx| root.confirm_update_check(window, cx))
                })
                .title(app_strong_text("Updates"))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, Self::close_update_check);
                })
                .child(div().w(secondary_dialog_content_width(width)).child(body))
                .footer(footer)
        });
        // Opening focuses the dialog, so this is its own focus handle.
        if let Some(dialog) = self.update_check.dialog.as_mut() {
            dialog.focus = window.focused(cx).as_ref().map(FocusHandle::downgrade);
        }
        cx.notify();
    }

    /// Enter and the primary footer button. Starting a check swaps the footer, so the focus moves
    /// back to the dialog for Escape and Enter to keep working. Enter on a result closes.
    fn confirm_update_check(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        match self
            .update_check
            .dialog
            .as_ref()
            .map(|dialog| &dialog.state)
        {
            Some(UpdateCheckState::Disclosure | UpdateCheckState::Failed(_)) => {
                self.start_update_check(cx);
                if let Some(focus) = self
                    .update_check
                    .dialog
                    .as_ref()
                    .and_then(|dialog| dialog.focus.as_ref()?.upgrade())
                {
                    focus.focus(window, cx);
                }
                false
            }
            Some(UpdateCheckState::Done(_)) => true,
            Some(UpdateCheckState::Checking { .. }) | None => false,
        }
    }

    /// Confirm and Retry: abort a running check and send a new one under a new generation.
    pub(super) fn start_update_check(&mut self, cx: &mut Context<'_, Self>) {
        if self.update_check.dialog.is_none() {
            return;
        }
        self.update_check.abort_running();
        // Debug UI fixture: an `update-check:` mode shows its canned result and sends nothing.
        #[cfg(debug_assertions)]
        {
            if let Some(result) = super::private_swap::ui_fixture_update_check()
                && let Some(dialog) = self.update_check.dialog.as_mut()
            {
                dialog.show(result);
                cx.notify();
                return;
            }
        }
        self.update_check.generation += 1;
        let generation = self.update_check.generation;
        let http = self.http.clone();
        let join = self
            .runtime
            .spawn(async move { check_releases(&http, BUILD_VERSION).await });
        if let Some(dialog) = self.update_check.dialog.as_mut() {
            dialog.state = UpdateCheckState::Checking {
                generation,
                abort: join.abort_handle(),
            };
        }
        cx.spawn(async move |this, cx| {
            let result = join.await.unwrap_or_else(|_| {
                Err(ReleaseCheckError {
                    reason: "the check stopped".to_owned(),
                })
            });
            let _ = this.update(cx, |root, cx| {
                root.apply_update_check_result(generation, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Shows a check's result only while the dialog still waits for that check: a result can
    /// already be on its way back when the check is aborted.
    fn apply_update_check_result(
        &mut self,
        generation: u64,
        result: Result<ReleaseTimeline, ReleaseCheckError>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(dialog) = self.update_check.dialog.as_mut() else {
            return;
        };
        if !matches!(
            dialog.state,
            UpdateCheckState::Checking { generation: running, .. } if running == generation
        ) {
            return;
        }
        dialog.show(result);
        cx.notify();
    }

    fn close_update_check(&mut self, cx: &mut Context<'_, Self>) {
        self.update_check.abort_running();
        self.update_check.dialog = None;
        cx.notify();
    }

    fn toggle_update_check_entry(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        if let Some(dialog) = self.update_check.dialog.as_mut() {
            if !dialog.expanded.remove(&index) {
                dialog.expanded.insert(index);
            }
            cx.notify();
        }
    }

    fn render_update_check_body(&self, root: &Entity<Self>) -> AnyElement {
        let Some(dialog) = &self.update_check.dialog else {
            return div().into_any_element();
        };
        let mode = self.http.network_mode();
        match &dialog.state {
            UpdateCheckState::Disclosure => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(app_text(disclosure_route(mode)))
                .into_any_element(),
            UpdateCheckState::Checking { .. } => div()
                .flex()
                .items_center()
                .gap_2()
                .child(Spinner::new().small())
                .child(app_muted_shimmer_text("Checking GitHub for releases…"))
                .into_any_element(),
            UpdateCheckState::Failed(error) => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(app_strong_text("Couldn't check for updates"))
                .child(app_muted_text(error.reason.clone()))
                .when(mode == WalletNetworkMode::Tor, |this| {
                    this.child(app_muted_text(
                        "GitHub may be limiting requests from Tor. Trying again will use a \
                         new circuit.",
                    ))
                })
                .into_any_element(),
            UpdateCheckState::Done(timeline) => {
                render_update_check_result(root, timeline, &dialog.expanded)
            }
        }
    }

    fn render_update_check_footer(&self, root: &Entity<Self>) -> AnyElement {
        let footer = DialogFooter::new();
        let footer = match self
            .update_check
            .dialog
            .as_ref()
            .map(|dialog| &dialog.state)
        {
            Some(UpdateCheckState::Disclosure) => footer
                .child(close_button(root, "Cancel").ghost())
                .child(start_button(root, CHECK_FOR_UPDATES)),
            Some(UpdateCheckState::Failed(_)) => footer
                .child(close_button(root, "Close").ghost())
                .child(start_button(root, "Retry")),
            Some(UpdateCheckState::Checking { .. }) => footer.child(close_button(root, "Cancel")),
            Some(UpdateCheckState::Done(_)) | None => footer.child(close_button(root, "Close")),
        };
        footer.into_any_element()
    }
}

fn disclosure_route(mode: WalletNetworkMode) -> String {
    match mode {
        WalletNetworkMode::Tor => "Checks GitHub for new releases over an isolated Tor circuit.",
        WalletNetworkMode::Proxy => "Checks GitHub for new releases using your configured proxy.",
        WalletNetworkMode::Direct => {
            "Checks GitHub for new releases directly, without Tor or a proxy."
        }
    }
    .to_owned()
}

fn close_button(root: &Entity<WalletRoot>, label: &'static str) -> Button {
    let root = root.clone();
    app_button("wallet-update-check-close", label).on_click(move |_event, window, cx| {
        root.update(cx, WalletRoot::close_update_check);
        window.close_dialog(cx);
    })
}

fn start_button(root: &Entity<WalletRoot>, label: &'static str) -> Button {
    let root = root.clone();
    app_button("wallet-update-check-start", label)
        .primary()
        .on_click(move |_event, window, cx| {
            root.update(cx, |root, cx| root.confirm_update_check(window, cx));
        })
}

/// The headline, with an icon and color only when the user is up to date or behind.
fn result_status(timeline: &ReleaseTimeline) -> (String, Option<(IconName, u32)>) {
    if timeline.entries.is_empty() {
        return ("No releases found.".to_owned(), None);
    }
    // `WARNING` is too close to body text to stand out as a headline.
    let behind = Some((IconName::TriangleAlert, theme::WARNING_STRONG));
    match (timeline.newer_count, timeline.installed) {
        (1, _) => ("A newer release is available.".to_owned(), behind),
        (newer, _) if newer > 1 => (format!("{newer} newer releases are available."), behind),
        (_, InstalledMarker::At(_)) => (
            "You're on the latest release.".to_owned(),
            Some((IconName::CircleCheck, theme::SUCCESS)),
        ),
        (_, InstalledMarker::Before(_)) => (
            "Your version is newer than the latest release.".to_owned(),
            None,
        ),
        (_, InstalledMarker::Unknown) => (
            "Couldn't determine whether an update is available.".to_owned(),
            None,
        ),
    }
}

fn render_result_status(timeline: &ReleaseTimeline) -> gpui::Div {
    let (text, tone) = result_status(timeline);
    let text = app_strong_text(text);
    match tone {
        Some((icon, color)) => div()
            .flex()
            .items_center()
            .gap_2()
            .child(Icon::new(icon).text_color(rgb(color)).flex_none())
            .child(text.text_color(rgb(color))),
        None => div().child(text),
    }
}

fn render_update_check_result(
    root: &Entity<WalletRoot>,
    timeline: &ReleaseTimeline,
    expanded: &BTreeSet<usize>,
) -> AnyElement {
    let mut rows = div().w_full().flex().flex_col().gap_2();
    for (index, entry) in timeline.entries.iter().enumerate() {
        if matches!(
            timeline.installed,
            InstalledMarker::At(at) | InstalledMarker::Before(at) if at == index
        ) {
            rows = rows.child(render_installed_marker());
        }
        rows = rows.child(render_release_entry(
            root,
            index,
            entry,
            index < timeline.newer_count,
            expanded.contains(&index),
        ));
    }
    if timeline.installed == InstalledMarker::Before(timeline.entries.len()) {
        rows = rows.child(render_installed_marker());
    }
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_result_status(timeline))
        .children(timeline.latest().map(render_latest_release))
        .when(!timeline.entries.is_empty(), |this| this.child(rows))
        .into_any_element()
}

fn render_latest_release(latest: &GithubRelease) -> gpui::Div {
    let url = SharedString::from(latest.html_url.clone());
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(format!(
            "Latest release: {}",
            latest.tag_name
        )))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    app_text(url.clone())
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(12.0))
                        .min_w_0()
                        .truncate(),
                )
                .child(
                    clipboard_with_toast("wallet-update-check-latest-url-copy", url)
                        .tooltip("Copy release link"),
                ),
        )
}

fn render_installed_marker() -> gpui::Div {
    let rule = || div().flex_1().h(px(1.0)).bg(rgb(theme::BORDER));
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(rule())
        .child(
            app_muted_text(format!("Installed v{BUILD_VERSION}"))
                .text_size(px(12.0))
                .flex_none(),
        )
        .child(rule())
}

fn render_release_entry(
    root: &Entity<WalletRoot>,
    index: usize,
    entry: &GithubRelease,
    newer: bool,
    expanded: bool,
) -> gpui::Div {
    let tag = entry.tag_name.as_str();
    let toggle_root = root.clone();
    let toggle = app_button_base(SharedString::from(format!(
        "wallet-update-check-toggle-{tag}"
    )))
    .text()
    .small()
    .icon(if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .accessibility_label(if expanded {
        format!("Hide notes for {tag}")
    } else {
        format!("Show notes for {tag}")
    })
    .when(!newer, |this| this.text_color(rgb(theme::TEXT_MUTED)))
    .child(app_button_label(tag.to_owned()))
    .on_click(move |_event, _window, cx| {
        toggle_root.update(cx, |root, cx| root.toggle_update_check_entry(index, cx));
    });
    let name = entry
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != tag);
    let date = entry
        .published_at
        .as_deref()
        .and_then(|published| published.split('T').next());
    let header = div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(toggle.flex_none())
        .child(
            app_muted_text(name.unwrap_or_default().to_owned())
                .flex_1()
                .min_w_0()
                .truncate(),
        )
        .children(date.map(|date| {
            app_muted_text(date.to_owned())
                .text_size(px(12.0))
                .flex_none()
        }));
    div()
        .w_full()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .map(|this| {
            if newer {
                this.border_color(rgb_with_alpha(theme::PRIMARY, 0.45))
                    .bg(rgb_with_alpha(theme::PRIMARY, 0.06))
            } else {
                this.border_color(rgb(theme::BORDER_SUBTLE))
                    .text_color(rgb(theme::TEXT_MUTED))
            }
        })
        .child(
            Collapsible::new()
                .open(expanded)
                .w_full()
                .child(header)
                .when(expanded, |this| {
                    this.content(
                        div()
                            .w_full()
                            .min_w_0()
                            .pt_2()
                            // Notes read as body text even inside an older, muted entry.
                            .text_color(rgb(theme::TEXT))
                            .child(render_release_notes(tag, entry.body.as_deref())),
                    )
                }),
        )
}

/// Notes render as inert Markdown. Notes the sanitizer can't make inert show as plain text in
/// a code block.
fn render_release_notes(tag: &str, body: Option<&str>) -> AnyElement {
    let Some(body) = body.map(str::trim).filter(|body| !body.is_empty()) else {
        return app_muted_text("No release notes").into_any_element();
    };
    let source = sanitize_render_source(body).unwrap_or_else(|| {
        let fence = "`".repeat(max_backtick_run(body).saturating_add(1).max(3));
        format!("{fence}\n{body}\n{fence}")
    });
    TextView::markdown(
        SharedString::from(format!("wallet-update-check-notes-{tag}")),
        source,
    )
    .selectable(true)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use wallet_ops::release_check::{GithubRelease, ReleaseCheckError, classify};

    use super::{UpdateCheckDialog, UpdateCheckState, WalletRoot, initial_expansion};

    fn release(tag: &str) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_owned(),
            name: None,
            body: None,
            html_url: String::new(),
            published_at: None,
            prerelease: false,
            draft: false,
        }
    }

    #[test]
    fn initial_expansion_opens_newer_releases_or_the_installed_one() {
        let releases = || {
            ["v1.3.0", "v1.2.0", "v1.1.0", "v1.0.0"]
                .map(release)
                .to_vec()
        };
        assert_eq!(
            initial_expansion(&classify("1.1.0", releases())),
            BTreeSet::from([0, 1])
        );
        assert_eq!(
            initial_expansion(&classify("1.3.0", releases())),
            BTreeSet::from([0])
        );
        assert!(initial_expansion(&classify("2.0.0", releases())).is_empty());
    }

    fn drive(cx: &gpui::VisualTestContext, runtime: &tokio::runtime::Runtime) {
        runtime.block_on(tokio::time::sleep(Duration::from_millis(50)));
        cx.run_until_parked();
    }

    fn checking(root: &WalletRoot) -> Option<(u64, tokio::task::AbortHandle)> {
        match &root.update_check.dialog {
            Some(UpdateCheckDialog {
                state: UpdateCheckState::Checking { generation, abort },
                ..
            }) => Some((*generation, abort.clone())),
            _ => None,
        }
    }

    fn stale_result() -> Result<wallet_ops::release_check::ReleaseTimeline, ReleaseCheckError> {
        Err(ReleaseCheckError {
            reason: "stale".to_owned(),
        })
    }

    #[gpui::test]
    fn update_check_sends_only_on_confirm_and_drops_abandoned_results(
        cx: &mut gpui::TestAppContext,
    ) {
        let path = std::env::temp_dir().join(format!(
            "railoxide-update-check-{:032x}",
            rand::random::<u128>()
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        // A proxy that takes connections and never answers holds the request open.
        let silent_proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_url = format!("http://{}", silent_proxy.local_addr().unwrap());
        let opens = Arc::new(AtomicUsize::new(0));
        let http = {
            let opens = Arc::clone(&opens);
            wallet_ops::HttpContext::with_rpc_client_for_tests(
                reqwest::Client::new(),
                wallet_ops::WalletNetworkMode::Direct,
            )
            .with_operation_clients_for_tests(move || {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(wallet_ops::OperationHttpClient::for_tests(
                    reqwest::Client::builder()
                        .proxy(reqwest::Proxy::all(&proxy_url)?)
                        .build()?,
                    wallet_ops::OperationNetworkIsolation::Unavailable(
                        wallet_ops::WalletNetworkMode::Direct,
                    ),
                ))
            })
        };
        cx.update(gpui_component::init);
        let mut root = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let wallet =
                crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
            root = Some(wallet.clone());
            gpui_component::Root::new(wallet, window, cx)
        });
        let root = root.unwrap();
        root.update(cx, |root, _| root.http = http);

        // Opening the dialog sends nothing.
        root.update_in(cx, |root, window, cx| {
            root.open_update_check_dialog(window, cx);
        });
        drive(cx, &runtime);
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        // Enter confirms and opens one operation client, whose request waits on the silent proxy.
        cx.simulate_keystrokes("enter");
        drive(cx, &runtime);
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        let (generation, abort) = root
            .read_with(cx, |root, _| checking(root))
            .expect("the check is running");
        assert!(!abort.is_finished());

        // A result of an earlier generation leaves the running check on screen.
        root.update(cx, |root, cx| {
            root.apply_update_check_result(generation - 1, stale_result(), cx);
        });
        assert!(root.read_with(cx, |root, _| checking(root)).is_some());

        // A programmatic dismissal also aborts the check, even when another dialog replaces it,
        // and the check's result is dropped when it arrives.
        cx.update(|window, cx| {
            gpui_component::WindowExt::close_all_dialogs(window, cx);
            ui::dialog::WalletDialogExt::open_wallet_dialog(window, cx, |dialog, _, _| dialog);
        });
        drive(cx, &runtime);
        assert!(abort.is_finished());
        root.update(cx, |root, cx| {
            root.apply_update_check_result(generation, stale_result(), cx);
        });
        assert!(root.read_with(cx, |root, _| root.update_check.dialog.is_none()));
        let _ = std::fs::remove_dir_all(&path);
    }
}
