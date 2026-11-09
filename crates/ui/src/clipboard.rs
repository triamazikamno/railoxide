//! Shared copy affordance for GPUI surfaces.

use std::time::Duration;

use gpui::{App, ClipboardItem, ElementId, Global, SharedString, Window};
use gpui_component::{WindowExt, clipboard::Clipboard, notification::Notification};

const COPIED_MESSAGE: &str = "Copied to clipboard!";
const SECRET_CLIPBOARD_LIFETIME: Duration = Duration::from_secs(30);

/// Counts secret copies so only the latest copy's timer clears, and whether a clear is pending.
#[derive(Default)]
struct CopiedSecret {
    generation: u64,
    pending: bool,
}

impl Global for CopiedSecret {}

/// Render a copy button that writes to the clipboard and shows a confirmation toast.
pub fn clipboard_with_toast(id: impl Into<ElementId>, value: impl Into<SharedString>) -> Clipboard {
    Clipboard::new(id)
        .value(value)
        .on_copied(|_value, window, cx| {
            push_copied_toast(window, cx);
        })
}

/// Like [`clipboard_with_toast`] for secrets: the clipboard is cleared after 30 seconds or by
/// [`clear_copied_secret`]. Clipboard history and sync tools that already captured it keep it.
pub fn secret_clipboard_with_toast(
    id: impl Into<ElementId>,
    value: impl Into<SharedString>,
) -> Clipboard {
    Clipboard::new(id)
        .value(value)
        .on_copied(|_value, window, cx| {
            remember_copied_secret(cx);
            push_copied_toast(window, cx);
        })
}

fn remember_copied_secret(cx: &mut App) {
    let secret = cx.default_global::<CopiedSecret>();
    secret.generation = secret.generation.wrapping_add(1);
    secret.pending = true;
    let generation = secret.generation;
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(SECRET_CLIPBOARD_LIFETIME)
            .await;
        cx.update(|cx| {
            if cx.global::<CopiedSecret>().generation == generation {
                clear_copied_secret(cx);
            }
        });
    })
    .detach();
}

/// Clear the clipboard if a copied secret is pending. The clipboard is not read first, so as not
/// to inspect other apps' data or trigger OS paste alerts; this also wipes anything copied after
/// the secret. A write the platform skips (an unfocused Wayland window, a busy Windows clipboard)
/// is not retried.
pub fn clear_copied_secret(cx: &mut App) {
    if cx
        .try_global::<CopiedSecret>()
        .is_some_and(|secret| secret.pending)
    {
        cx.global_mut::<CopiedSecret>().pending = false;
        cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
    }
}

/// Copy text to the clipboard and show the same confirmation toast as [`clipboard_with_toast`].
pub fn copy_to_clipboard_with_toast(
    value: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    copy_to_clipboard_with_custom_toast(value, COPIED_MESSAGE, window, cx);
}

/// Copy text to the clipboard and show a custom confirmation toast.
pub fn copy_to_clipboard_with_custom_toast(
    value: impl Into<SharedString>,
    message: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let value = value.into();
    cx.write_to_clipboard(ClipboardItem::new_string(value.to_string()));
    push_copied_toast_message(message, window, cx);
}

fn push_copied_toast(window: &mut Window, cx: &mut App) {
    push_copied_toast_message(COPIED_MESSAGE, window, cx);
}

fn push_copied_toast_message(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    window.push_notification(Notification::success(message), cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn clipboard_text(cx: &TestAppContext) -> Option<String> {
        cx.read_from_clipboard().and_then(|item| item.text())
    }

    fn copy_secret(cx: &TestAppContext, secret: &'static str) {
        cx.write_to_clipboard(ClipboardItem::new_string(secret.to_owned()));
        cx.update(remember_copied_secret);
    }

    #[gpui::test]
    fn each_copied_secret_expires_at_its_own_deadline(cx: &TestAppContext) {
        copy_secret(cx, "first secret");
        cx.executor().advance_clock(Duration::from_secs(29));
        assert_eq!(clipboard_text(cx).as_deref(), Some("first secret"));

        copy_secret(cx, "second secret");
        cx.executor().advance_clock(Duration::from_secs(1));
        assert_eq!(clipboard_text(cx).as_deref(), Some("second secret"));

        cx.executor().advance_clock(Duration::from_secs(29));
        assert_eq!(clipboard_text(cx).unwrap_or_default(), "");
    }
}
