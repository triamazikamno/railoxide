use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;
use broadcaster_monitor_waku::{DEFAULT_DOH_ENDPOINT, DEFAULT_TOR_DOH_ENDPOINT};
use gpui::{
    App, AppContext as _, Axis, Context, ElementId, Entity, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, IndexPath, Sizable, WindowExt,
    alert::Alert,
    button::{Button, ButtonVariants},
    group_box::GroupBoxVariant,
    input::{Input, InputEvent, InputState, NumberInput, NumberInputEvent, StepAction},
    label::Label,
    select::{Select, SelectDelegate, SelectEvent, SelectItem, SelectState},
    setting::{
        NumberFieldOptions, SelectIndex, SettingField, SettingGroup, SettingItem, SettingPage,
        Settings as ComponentSettings,
    },
    slider::{Slider, SliderEvent, SliderState},
    switch::Switch,
};
use railgun_ui::{chain_icon_asset_path, chain_name, short_address};
use tokio::runtime::Handle;
use tokio::sync::watch;
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text, app_text};
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::{
    HttpContext, ProverCacheBuildProgress, WALLETCONNECT_DEFAULT_PROJECT_ID, WalletDbStore,
    WalletNetworkConfig, WalletNetworkMode, begin_prover_cache_build,
    build_cache_with_context_and_progress_with_session, build_wallet_network_context,
    settings::{
        AUTO_LOCK_TIMEOUT_PRESETS_SECS, BuiltInTokenOverride, CustomTokenSettings,
        IndexedArtifactManifestSourceSetting, IndexedArtifactSettings,
        IndexedArtifactSourceModeSetting, NetworkModeSetting, PoiReadSourceSetting,
        PriceAnchorSettings, TokenKey, TokenPriceAnchorOverride, WakuDirectPeerSetting,
        WalletSettings, build_effective_chain_configs, build_effective_token_registry,
        default_token_price_anchor_overrides, default_waku_backup_peers, default_waku_direct_peers,
        default_waku_dns_enr_trees,
    },
    vault::DesktopVaultStore,
};

use crate::assets::RailgunActionIcon;

use super::WalletRoot;
use super::maintenance::{WalletMaintenanceController, WalletMaintenanceReset};
use super::startup::WalletStartupRoot;
use super::ui_helpers::{
    ConfirmationDialogProps, confirmation_dialog, dialog_max_height, rgb_with_alpha,
    secondary_dialog_content_width,
};
use super::wallet_header::ChainSelectItem;

mod apply_mode;
mod chains;
mod editor;
mod network;
mod render;
mod root;
mod tokens;
mod types;
mod ui_helpers;

pub(super) use apply_mode::*;
pub(super) use network::*;
pub(super) use tokens::*;
pub(super) use types::*;
pub(super) use ui_helpers::*;
