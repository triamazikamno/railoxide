use super::WalletRoot;
use super::walletconnect::walletconnect_attention_transition;

impl WalletRoot {
    /// Pending `WalletConnect` items plus open spend authorization dialogs.
    pub(super) fn sync_platform_attention(&mut self) {
        self.spend_authorization_attention
            .retain(|open| open.strong_count() > 0);
        let next_count =
            self.walletconnect.attention_count() + self.spend_authorization_attention.len();
        let transition = walletconnect_attention_transition(
            self.platform_attention_count,
            next_count,
            self.platform_attention_window_active,
        );
        if transition.sync_badge_count {
            self.platform_attention.sync_badge_count(next_count);
        }
        if transition.request_attention {
            self.platform_attention.request_attention();
        }
        if transition.clear_attention {
            self.platform_attention.clear_attention();
        }
        self.platform_attention_count = next_count;
    }

    pub(super) fn sync_platform_attention_for_window(&mut self, window: &gpui::Window) {
        self.platform_attention_window_active = window.is_window_active();
        self.sync_platform_attention();
    }
}

pub(super) struct PlatformAttentionState {
    native: imp::NativePlatformAttentionState,
}

impl PlatformAttentionState {
    // The Windows implementation resolves a native handle at runtime and cannot be const.
    #[allow(clippy::missing_const_for_fn)]
    pub(super) fn new(window: &gpui::Window) -> Self {
        Self {
            native: imp::NativePlatformAttentionState::new(window),
        }
    }

    #[allow(clippy::missing_const_for_fn)]
    pub(super) fn sync_badge_count(&self, count: usize) {
        self.native.sync_badge_count(count);
    }

    #[allow(clippy::missing_const_for_fn, clippy::needless_pass_by_ref_mut)]
    pub(super) fn request_attention(&mut self) {
        self.native.request_attention();
    }

    #[allow(clippy::missing_const_for_fn, clippy::needless_pass_by_ref_mut)]
    pub(super) fn clear_attention(&mut self) {
        self.native.clear_attention();
    }
}

impl Drop for PlatformAttentionState {
    fn drop(&mut self) {
        self.sync_badge_count(0);
        self.clear_attention();
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSRequestUserAttentionType};
    use objc2_foundation::{NSInteger, NSString};

    pub(super) struct NativePlatformAttentionState {
        attention_request_id: Option<NSInteger>,
    }

    impl NativePlatformAttentionState {
        pub(super) const fn new(_window: &gpui::Window) -> Self {
            Self {
                attention_request_id: None,
            }
        }

        #[allow(clippy::unused_self)]
        pub(super) fn sync_badge_count(&self, count: usize) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
            let dock_tile = NSApplication::sharedApplication(mtm).dockTile();
            dock_tile.setBadgeLabel(label.as_deref());
            dock_tile.display();
        }

        pub(super) fn request_attention(&mut self) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let request_id = NSApplication::sharedApplication(mtm)
                .requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
            self.attention_request_id = Some(request_id);
        }

        pub(super) fn clear_attention(&mut self) {
            let Some(request_id) = self.attention_request_id.take() else {
                return;
            };
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            NSApplication::sharedApplication(mtm).cancelUserAttentionRequest(request_id);
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::mem::size_of;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{FLASHW_STOP, FLASHW_TRAY, FLASHWINFO, FlashWindowEx},
    };

    pub(super) struct NativePlatformAttentionState {
        hwnd: Option<HWND>,
    }

    impl NativePlatformAttentionState {
        pub(super) fn new(window: &gpui::Window) -> Self {
            Self {
                hwnd: hwnd_for_window(window),
            }
        }

        pub(super) fn sync_badge_count(&self, _count: usize) {}

        pub(super) fn request_attention(&mut self) {
            let Some(hwnd) = self.hwnd else {
                return;
            };
            flash_window(hwnd, FLASHW_TRAY, 3);
        }

        pub(super) fn clear_attention(&mut self) {
            let Some(hwnd) = self.hwnd else {
                return;
            };
            flash_window(hwnd, FLASHW_STOP, 0);
        }
    }

    fn hwnd_for_window(window: &gpui::Window) -> Option<HWND> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
            _ => None,
        }
    }

    fn flash_window(hwnd: HWND, flags: u32, count: u32) {
        let mut info = FLASHWINFO {
            cbSize: size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: flags,
            uCount: count,
            dwTimeout: 0,
        };
        unsafe {
            let _ = FlashWindowEx(&raw mut info);
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    pub(super) struct NativePlatformAttentionState;

    impl NativePlatformAttentionState {
        pub(super) const fn new(_window: &gpui::Window) -> Self {
            Self
        }

        #[allow(clippy::unused_self)]
        pub(super) const fn sync_badge_count(&self, _count: usize) {}

        #[allow(clippy::unused_self)]
        pub(super) const fn request_attention(&self) {}

        #[allow(clippy::unused_self)]
        pub(super) const fn clear_attention(&self) {}
    }
}
