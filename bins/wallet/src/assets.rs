use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{AssetSource, ImageSource, Result, SharedString};
use gpui_component::IconNamed;
use rust_embed::RustEmbed;

pub(crate) const LOGO_ICON_PATH: &str = "railgun/icons/logo.svg";
pub(crate) const SIDEBAR_WORDMARK_PATH: &str = "railgun/icons/wordmark.svg";
pub(crate) const HERO_WORDMARK_PATH: &str = "railgun/icons/hero-wordmark.svg";
pub(crate) const HEMATITE_HERO_PATH: &str = "railgun/backgrounds/hematite-hero.svg";
pub(crate) const WARM_GLOW_PATH: &str = "railgun/backgrounds/warm-glow.svg";
pub(crate) const LEDGER_LOGO_SHORT_WHITE_ICON_PATH: &str =
    "railgun/icons/ledger-logo-short-white.svg";
pub(crate) const TREZOR_SYMBOL_WHITE_ICON_PATH: &str = "railgun/icons/trezor-symbol-white-rgb.svg";
pub(crate) const WALLETCONNECT_ICON_PATH: &str = "railgun/icons/walletconnect.svg";
const TELEGRAM_ICON_PATH: &str = "railgun/icons/telegram.svg";
const ARROW_BIG_RIGHT_DASH_ICON_PATH: &str = "railgun/icons/arrow-big-right-dash.svg";
const SHIELD_ICON_PATH: &str = "railgun/icons/shield.svg";
const WALLET_ICON_PATH: &str = "railgun/icons/wallet.svg";
const BROADCASTER_ICON_PATH: &str = "railgun/icons/robot.svg";
const LOGS_ICON_PATH: &str = "railgun/icons/logs.svg";
const DICES_ICON_PATH: &str = "railgun/icons/dices.svg";
const SQUARE_ICON_PATH: &str = "railgun/icons/square.svg";
const SQUARE_ASTERISK_ICON_PATH: &str = "railgun/icons/square-asterisk.svg";
const PENCIL_ICON_PATH: &str = "railgun/icons/pencil.svg";
const QR_CODE_ICON_PATH: &str = "railgun/icons/qr-code.svg";
const TRASH_2_ICON_PATH: &str = "railgun/icons/trash-2.svg";
const CLOCK_ICON_PATH: &str = "railgun/icons/clock.svg";
const BOOK_USER_ICON_PATH: &str = "railgun/icons/book-user.svg";
const LANDMARK_ICON_PATH: &str = "railgun/icons/landmark.svg";
const SAVE_ICON_PATH: &str = "railgun/icons/save.svg";
pub(crate) const IMPORT_ICON_PATH: &str = "railgun/icons/import.svg";
pub(crate) const LIST_ICON_PATH: &str = "railgun/icons/list.svg";
pub(crate) const GROUP_ICON_PATH: &str = "railgun/icons/group.svg";
pub(crate) const USERS_ICON_PATH: &str = "railgun/icons/users.svg";
pub(crate) const PIGGY_BANK_ICON_PATH: &str = "railgun/icons/piggy-bank.svg";
pub(crate) const CHEVRONS_DOWN_ICON_PATH: &str = "railgun/icons/chevrons-down.svg";
const KEY_ROUND_ICON_PATH: &str = "railgun/icons/key-round.svg";
const WRENCH_ICON_PATH: &str = "railgun/icons/wrench.svg";
const FILE_PEN_LINE_ICON_PATH: &str = "railgun/icons/file-pen-line.svg";
const FOLDER_COG_ICON_PATH: &str = "railgun/icons/folder-cog.svg";
const SPARKLES_ICON_PATH: &str = "railgun/icons/sparkles.svg";
const NETWORK_ICON_PATH: &str = "railgun/icons/network.svg";
const PIN_ICON_PATH: &str = "railgun/icons/pin.svg";
const TOR_STATUS_ICON_PATH: &str = "railgun/icons/tor-status.svg";
const ARROW_RIGHT_LEFT_ICON_PATH: &str = "railgun/icons/arrow-right-left.svg";
const ARROW_DOWN_TO_LINE_ICON_PATH: &str = "railgun/icons/arrow-down-to-line.svg";
const UI_ASSET_PREFIX: &str = "ui/";
const RAILGUN_UI_ASSET_PREFIX: &str = "railgun-ui/";

const RAILGUN_ASSET_PATHS: &[&str] = &[
    LOGO_ICON_PATH,
    SIDEBAR_WORDMARK_PATH,
    HERO_WORDMARK_PATH,
    HEMATITE_HERO_PATH,
    WARM_GLOW_PATH,
    LEDGER_LOGO_SHORT_WHITE_ICON_PATH,
    TREZOR_SYMBOL_WHITE_ICON_PATH,
    WALLETCONNECT_ICON_PATH,
    TELEGRAM_ICON_PATH,
    ARROW_BIG_RIGHT_DASH_ICON_PATH,
    SHIELD_ICON_PATH,
    WALLET_ICON_PATH,
    BROADCASTER_ICON_PATH,
    LOGS_ICON_PATH,
    DICES_ICON_PATH,
    SQUARE_ICON_PATH,
    SQUARE_ASTERISK_ICON_PATH,
    PENCIL_ICON_PATH,
    QR_CODE_ICON_PATH,
    TRASH_2_ICON_PATH,
    CLOCK_ICON_PATH,
    BOOK_USER_ICON_PATH,
    LANDMARK_ICON_PATH,
    SAVE_ICON_PATH,
    IMPORT_ICON_PATH,
    LIST_ICON_PATH,
    GROUP_ICON_PATH,
    USERS_ICON_PATH,
    PIGGY_BANK_ICON_PATH,
    CHEVRONS_DOWN_ICON_PATH,
    KEY_ROUND_ICON_PATH,
    WRENCH_ICON_PATH,
    FILE_PEN_LINE_ICON_PATH,
    FOLDER_COG_ICON_PATH,
    SPARKLES_ICON_PATH,
    NETWORK_ICON_PATH,
    PIN_ICON_PATH,
    TOR_STATUS_ICON_PATH,
    ARROW_RIGHT_LEFT_ICON_PATH,
    ARROW_DOWN_TO_LINE_ICON_PATH,
];

const LOGO_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/logo.svg");
const SIDEBAR_WORDMARK_BYTES: &[u8] = include_bytes!("../assets/icons/wordmark.svg");
const HERO_WORDMARK_BYTES: &[u8] = include_bytes!("../assets/icons/hero-wordmark.svg");
const HEMATITE_HERO_BYTES: &[u8] = include_bytes!("../assets/backgrounds/hematite-hero.svg");
const WARM_GLOW_BYTES: &[u8] = include_bytes!("../assets/backgrounds/warm-glow.svg");
const LEDGER_LOGO_SHORT_WHITE_ICON_BYTES: &[u8] =
    include_bytes!("../assets/icons/ledger-logo-short-white.svg");
const TREZOR_SYMBOL_WHITE_ICON_BYTES: &[u8] =
    include_bytes!("../assets/icons/trezor-symbol-white-rgb.svg");
const WALLETCONNECT_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/walletconnect.svg");
const TELEGRAM_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/telegram.svg");
const ARROW_BIG_RIGHT_DASH_ICON_BYTES: &[u8] =
    include_bytes!("../assets/icons/arrow-big-right-dash.svg");
const SHIELD_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/shield.svg");
const WALLET_ICON_BYTES: &[u8] = include_bytes!("../../../crates/ui/assets/icons/wallet.svg");
const BROADCASTER_ICON_BYTES: &[u8] = include_bytes!("../../../crates/ui/assets/icons/robot.svg");
const LOGS_ICON_BYTES: &[u8] = include_bytes!("../../../crates/ui/assets/icons/logs.svg");
const DICES_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/dices.svg");
const SQUARE_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/square.svg");
const SQUARE_ASTERISK_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/square-asterisk.svg");
const PENCIL_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/pencil.svg");
const QR_CODE_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/qr-code.svg");
const TRASH_2_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/trash-2.svg");
const CLOCK_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/clock.svg");
const BOOK_USER_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/book-user.svg");
const LANDMARK_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/landmark.svg");
const SAVE_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/save.svg");
const IMPORT_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/import.svg");
const LIST_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/list.svg");
const GROUP_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/group.svg");
const USERS_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/users.svg");
const PIGGY_BANK_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/piggy-bank.svg");
const CHEVRONS_DOWN_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/chevrons-down.svg");
const KEY_ROUND_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/key-round.svg");
const WRENCH_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/wrench.svg");
const FILE_PEN_LINE_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/file-pen-line.svg");
const FOLDER_COG_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/folder-cog.svg");
const SPARKLES_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/sparkles.svg");
const NETWORK_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/network.svg");
const PIN_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/pin.svg");
const TOR_STATUS_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/tor-status.svg");
const ARROW_RIGHT_LEFT_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/arrow-right-left.svg");
const ARROW_DOWN_TO_LINE_ICON_BYTES: &[u8] =
    include_bytes!("../assets/icons/arrow-down-to-line.svg");

pub(crate) struct WalletAssets;

#[derive(RustEmbed)]
#[folder = "../../crates/ui/assets"]
struct UiAssets;

#[derive(RustEmbed)]
#[folder = "../../crates/railgun-ui/assets"]
struct RailgunUiAssets;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WalletIconSource {
    Embedded(String),
    File(PathBuf),
}

impl WalletIconSource {
    pub(crate) fn embedded(path: impl Into<String>) -> Self {
        Self::Embedded(path.into())
    }

    pub(crate) fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }

    #[cfg(test)]
    pub(crate) fn as_file_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Embedded(_) => None,
            Self::File(path) => Some(path.as_path()),
        }
    }
}

impl From<WalletIconSource> for ImageSource {
    fn from(source: WalletIconSource) -> Self {
        match source {
            WalletIconSource::Embedded(path) => Self::from(path),
            WalletIconSource::File(path) => Self::from(path),
        }
    }
}

impl AssetSource for WalletAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let asset = if let Some(bytes) = railgun_asset(path) {
            Some(Cow::Borrowed(bytes))
        } else if let Some(bytes) = embedded_asset::<UiAssets>(UI_ASSET_PREFIX, path) {
            Some(bytes)
        } else if let Some(bytes) = embedded_asset::<RailgunUiAssets>(RAILGUN_UI_ASSET_PREFIX, path)
        {
            Some(bytes)
        } else {
            gpui_component_assets::Assets.load(path)?
        };

        #[cfg(feature = "heap-profiling")]
        if let Some(bytes) = asset.as_deref() {
            trace_image_asset(path, bytes);
        }
        Ok(asset)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets = gpui_component_assets::Assets.list(path)?;
        append_embedded_asset_list::<UiAssets>(&mut assets, UI_ASSET_PREFIX, path);
        append_embedded_asset_list::<RailgunUiAssets>(&mut assets, RAILGUN_UI_ASSET_PREFIX, path);
        assets.extend(
            RAILGUN_ASSET_PATHS
                .iter()
                .filter(|asset| asset.starts_with(path))
                .map(|asset| SharedString::from(*asset)),
        );
        Ok(assets)
    }
}

#[cfg(feature = "heap-profiling")]
fn trace_image_asset(path: &str, bytes: &[u8]) {
    let format = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if std::str::from_utf8(bytes).is_ok_and(|text| text.contains("<svg")) {
        "svg"
    } else {
        "other"
    };
    tracing::info!(
        target: "wallet::heap_profile",
        asset_path = path,
        encoded_bytes = bytes.len(),
        asset_format = format,
        svg_width = ?svg_attribute(bytes, "width"),
        svg_height = ?svg_attribute(bytes, "height"),
        svg_view_box = ?svg_attribute(bytes, "viewBox"),
        "loaded GPUI image asset source"
    );
}

#[cfg(feature = "heap-profiling")]
fn svg_attribute<'a>(bytes: &'a [u8], name: &str) -> Option<&'a str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let svg = &text[text.find("<svg")?..];
    let mut quote = None;
    let header_end = svg.char_indices().find_map(|(index, character)| {
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            return None;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
            None
        } else {
            (character == '>').then_some(index)
        }
    })?;
    let header = &svg[..header_end];
    let mut attributes = &header["<svg".len()..];
    loop {
        attributes = attributes.trim_start();
        if attributes.is_empty() {
            return None;
        }
        let attribute_end = attributes
            .find(|character: char| character.is_whitespace() || character == '=')
            .unwrap_or(attributes.len());
        let attribute_name = &attributes[..attribute_end];
        attributes = attributes[attribute_end..].trim_start();
        let Some(remainder) = attributes.strip_prefix('=') else {
            continue;
        };
        let remainder = remainder.trim_start();
        let quote = remainder.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let value = &remainder[quote.len_utf8()..];
        let end = value.find(quote)?;
        attributes = &value[end + quote.len_utf8()..];
        if attribute_name == name {
            return Some(&value[..end]);
        }
    }
}

fn embedded_asset<T: RustEmbed>(prefix: &str, path: &str) -> Option<Cow<'static, [u8]>> {
    let path = path.strip_prefix(prefix)?;
    T::get(path).map(|file| file.data)
}

fn append_embedded_asset_list<T: RustEmbed>(
    assets: &mut Vec<SharedString>,
    prefix: &str,
    path: &str,
) {
    assets.extend(T::iter().filter_map(|asset| {
        let asset = format!("{prefix}{asset}");
        asset.starts_with(path).then(|| SharedString::from(asset))
    }));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RailgunActionIcon {
    Wallet,
    Send,
    Shield,
    Dices,
    Square,
    SquareAsterisk,
    Pencil,
    QrCode,
    Trash2,
    Clock,
    BookUser,
    Save,
    KeyRound,
    Wrench,
    FilePenLine,
    FolderCog,
    Sparkles,
}

impl IconNamed for RailgunActionIcon {
    fn path(self) -> SharedString {
        match self {
            Self::Wallet => WALLET_ICON_PATH,
            Self::Send => ARROW_BIG_RIGHT_DASH_ICON_PATH,
            Self::Shield => SHIELD_ICON_PATH,
            Self::Dices => DICES_ICON_PATH,
            Self::Square => SQUARE_ICON_PATH,
            Self::SquareAsterisk => SQUARE_ASTERISK_ICON_PATH,
            Self::Pencil => PENCIL_ICON_PATH,
            Self::QrCode => QR_CODE_ICON_PATH,
            Self::Trash2 => TRASH_2_ICON_PATH,
            Self::Clock => CLOCK_ICON_PATH,
            Self::BookUser => BOOK_USER_ICON_PATH,
            Self::Save => SAVE_ICON_PATH,
            Self::KeyRound => KEY_ROUND_ICON_PATH,
            Self::Wrench => WRENCH_ICON_PATH,
            Self::FilePenLine => FILE_PEN_LINE_ICON_PATH,
            Self::FolderCog => FOLDER_COG_ICON_PATH,
            Self::Sparkles => SPARKLES_ICON_PATH,
        }
        .into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RailgunPublicAccountIcon {
    Derived,
    Global,
    Imported,
}

impl IconNamed for RailgunPublicAccountIcon {
    fn path(self) -> SharedString {
        match self {
            Self::Derived => NETWORK_ICON_PATH,
            Self::Global => PIN_ICON_PATH,
            Self::Imported => KEY_ROUND_ICON_PATH,
        }
        .into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RailgunSidebarIcon {
    Wallet,
    Broadcaster,
    BookUser,
    Landmark,
    Logs,
}

impl IconNamed for RailgunSidebarIcon {
    fn path(self) -> SharedString {
        match self {
            Self::Wallet => WALLET_ICON_PATH,
            Self::Broadcaster => BROADCASTER_ICON_PATH,
            Self::BookUser => BOOK_USER_ICON_PATH,
            Self::Landmark => LANDMARK_ICON_PATH,
            Self::Logs => LOGS_ICON_PATH,
        }
        .into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RailgunNetworkStatusIcon {
    Tor,
    ConnectionSetup,
    Download,
}

impl IconNamed for RailgunNetworkStatusIcon {
    fn path(self) -> SharedString {
        match self {
            Self::Tor => TOR_STATUS_ICON_PATH,
            Self::ConnectionSetup => ARROW_RIGHT_LEFT_ICON_PATH,
            Self::Download => ARROW_DOWN_TO_LINE_ICON_PATH,
        }
        .into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RailgunSocialIcon {
    Telegram,
}

impl IconNamed for RailgunSocialIcon {
    fn path(self) -> SharedString {
        match self {
            Self::Telegram => TELEGRAM_ICON_PATH,
        }
        .into()
    }
}

fn railgun_asset(path: &str) -> Option<&'static [u8]> {
    match path {
        LOGO_ICON_PATH => Some(LOGO_ICON_BYTES),
        SIDEBAR_WORDMARK_PATH => Some(SIDEBAR_WORDMARK_BYTES),
        HERO_WORDMARK_PATH => Some(HERO_WORDMARK_BYTES),
        HEMATITE_HERO_PATH => Some(HEMATITE_HERO_BYTES),
        WARM_GLOW_PATH => Some(WARM_GLOW_BYTES),
        LEDGER_LOGO_SHORT_WHITE_ICON_PATH => Some(LEDGER_LOGO_SHORT_WHITE_ICON_BYTES),
        TREZOR_SYMBOL_WHITE_ICON_PATH => Some(TREZOR_SYMBOL_WHITE_ICON_BYTES),
        WALLETCONNECT_ICON_PATH => Some(WALLETCONNECT_ICON_BYTES),
        TELEGRAM_ICON_PATH => Some(TELEGRAM_ICON_BYTES),
        ARROW_BIG_RIGHT_DASH_ICON_PATH => Some(ARROW_BIG_RIGHT_DASH_ICON_BYTES),
        SHIELD_ICON_PATH => Some(SHIELD_ICON_BYTES),
        WALLET_ICON_PATH => Some(WALLET_ICON_BYTES),
        BROADCASTER_ICON_PATH => Some(BROADCASTER_ICON_BYTES),
        LOGS_ICON_PATH => Some(LOGS_ICON_BYTES),
        DICES_ICON_PATH => Some(DICES_ICON_BYTES),
        SQUARE_ICON_PATH => Some(SQUARE_ICON_BYTES),
        SQUARE_ASTERISK_ICON_PATH => Some(SQUARE_ASTERISK_ICON_BYTES),
        PENCIL_ICON_PATH => Some(PENCIL_ICON_BYTES),
        QR_CODE_ICON_PATH => Some(QR_CODE_ICON_BYTES),
        TRASH_2_ICON_PATH => Some(TRASH_2_ICON_BYTES),
        CLOCK_ICON_PATH => Some(CLOCK_ICON_BYTES),
        BOOK_USER_ICON_PATH => Some(BOOK_USER_ICON_BYTES),
        LANDMARK_ICON_PATH => Some(LANDMARK_ICON_BYTES),
        SAVE_ICON_PATH => Some(SAVE_ICON_BYTES),
        IMPORT_ICON_PATH => Some(IMPORT_ICON_BYTES),
        LIST_ICON_PATH => Some(LIST_ICON_BYTES),
        GROUP_ICON_PATH => Some(GROUP_ICON_BYTES),
        USERS_ICON_PATH => Some(USERS_ICON_BYTES),
        PIGGY_BANK_ICON_PATH => Some(PIGGY_BANK_ICON_BYTES),
        CHEVRONS_DOWN_ICON_PATH => Some(CHEVRONS_DOWN_ICON_BYTES),
        KEY_ROUND_ICON_PATH => Some(KEY_ROUND_ICON_BYTES),
        WRENCH_ICON_PATH => Some(WRENCH_ICON_BYTES),
        FILE_PEN_LINE_ICON_PATH => Some(FILE_PEN_LINE_ICON_BYTES),
        FOLDER_COG_ICON_PATH => Some(FOLDER_COG_ICON_BYTES),
        SPARKLES_ICON_PATH => Some(SPARKLES_ICON_BYTES),
        NETWORK_ICON_PATH => Some(NETWORK_ICON_BYTES),
        PIN_ICON_PATH => Some(PIN_ICON_BYTES),
        TOR_STATUS_ICON_PATH => Some(TOR_STATUS_ICON_BYTES),
        ARROW_RIGHT_LEFT_ICON_PATH => Some(ARROW_RIGHT_LEFT_ICON_BYTES),
        ARROW_DOWN_TO_LINE_ICON_PATH => Some(ARROW_DOWN_TO_LINE_ICON_BYTES),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_assets_embed_shared_icon_sets() {
        let assets = WalletAssets;

        assert!(
            assets
                .load("ui/icons/refresh-ccw.svg")
                .expect("load ui icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun-ui/chains/ethereum.svg")
                .expect("load chain icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun-ui/tokens/1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")
                .expect("load token icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/clock.svg")
                .expect("load clock icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/book-user.svg")
                .expect("load book user icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/landmark.svg")
                .expect("load landmark icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/save.svg")
                .expect("load save icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/square-asterisk.svg")
                .expect("load square asterisk icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/wrench.svg")
                .expect("load wrench icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/file-pen-line.svg")
                .expect("load file pen line icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/folder-cog.svg")
                .expect("load folder cog icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/import.svg")
                .expect("load import icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/list.svg")
                .expect("load list icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/group.svg")
                .expect("load group icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/users.svg")
                .expect("load users icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/piggy-bank.svg")
                .expect("load piggy bank icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/chevrons-down.svg")
                .expect("load chevrons down icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/ledger-logo-short-white.svg")
                .expect("load ledger icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/trezor-symbol-white-rgb.svg")
                .expect("load trezor icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/walletconnect.svg")
                .expect("load walletconnect icon")
                .is_some()
        );
        assert!(
            assets
                .load("railgun/icons/telegram.svg")
                .expect("load telegram icon")
                .is_some()
        );
    }
}
