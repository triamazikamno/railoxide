// Hex color literals are intentionally written `0xRRGGBB` style.
#![allow(clippy::unreadable_literal)]
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod assets;
mod cli;
mod root;

use std::time::Duration;

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
use std::ffi::{CStr, CString, c_void};
#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
use std::os::unix::ffi::OsStrExt;
#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
use std::path::PathBuf;

use broadcaster_monitor::{DEFAULT_EVENT_CAPACITY, event_channel, shared};
use eyre::{Result, WrapErr};
use gpui::App;
use railgun_ui::DEFAULT_CHAINS;
use tracing::metadata::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};
use ui::logs::{DEFAULT_LOG_CAPACITY, LogStore, UiLogLayer};

use crate::assets::WalletAssets;
use crate::cli::Options;
use crate::root::{
    WalletAppOptions, install_utxo_navigation_bindings, install_wallet_action_bindings,
    open_wallet_window,
};

#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
struct Quit;

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
struct DumpHeapProfile;

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
#[derive(Clone, Copy, Debug)]
struct JemallocStats {
    allocated: usize,
    active: usize,
    resident: usize,
    retained: usize,
}

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
union JemallocConfigPointer {
    byte: &'static u8,
    character: &'static std::ffi::c_char,
}

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
static JEMALLOC_CONFIG_BYTES: &[u8] =
    b"prof:true,prof_active:true,prof_accum:false,lg_prof_sample:19\0";

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static JEMALLOC_MALLOC_CONF: Option<&'static std::ffi::c_char> = Some(unsafe {
    // SAFETY: The bytes are static, NUL-terminated, and only reinterpreted as C characters.
    JemallocConfigPointer {
        byte: &JEMALLOC_CONFIG_BYTES[0],
    }
    .character
});

const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<()> {
    let opts = Options::from_args();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("wallet-worker")
        .build()
        .wrap_err("build tokio runtime")?;
    let logs = LogStore::new(DEFAULT_LOG_CAPACITY);
    install_tracing(logs.clone())?;

    let runtime_handle = runtime.handle().clone();
    let monitor = shared();
    let (event_tx, event_rx) = event_channel(DEFAULT_EVENT_CAPACITY);

    let chain_ids = DEFAULT_CHAINS.to_vec();
    let runtime_guard = runtime.enter();
    let wallet_options = WalletAppOptions::try_from(opts)?;
    let application = gpui_kit::application().with_assets(WalletAssets);
    application.run(move |app: &mut App| {
        gpui_kit::init(app);
        assets::init_fonts(app).expect("load bundled fonts");
        ui::theme::apply_zenburn_component_theme(app);
        install_quit_behavior(app);
        #[cfg(all(target_os = "macos", feature = "heap-profiling"))]
        install_heap_profile_behavior(app);
        install_wallet_action_bindings(app);
        install_utxo_navigation_bindings(app);
        open_wallet_window(
            app,
            wallet_options.clone(),
            runtime_handle.clone(),
            monitor.clone(),
            event_tx,
            event_rx,
            &chain_ids,
            logs,
        );

        #[cfg(target_os = "macos")]
        app.activate(true);
    });

    drop(runtime_guard);
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    Ok(())
}

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
fn install_heap_profile_behavior(app: &mut App) {
    app.on_action(|_: &DumpHeapProfile, _| match dump_heap_profile() {
        Ok((path, stats)) => {
            if let Some(stats) = stats {
                tracing::info!(
                    path = %path.display(),
                    allocated_bytes = stats.allocated,
                    active_bytes = stats.active,
                    resident_bytes = stats.resident,
                    retained_bytes = stats.retained,
                    "dumped jemalloc heap profile"
                );
            } else {
                tracing::info!(path = %path.display(), "dumped jemalloc heap profile");
            }
        }
        Err(error) => tracing::error!(%error, "failed to dump jemalloc heap profile"),
    });
    app.bind_keys([gpui::KeyBinding::new(
        "cmd-alt-shift-h",
        DumpHeapProfile,
        None,
    )]);
}

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
fn dump_heap_profile() -> Result<(PathBuf, Option<JemallocStats>)> {
    let stats = match read_jemalloc_stats() {
        Ok(stats) => Some(stats),
        Err(error) => {
            tracing::warn!(%error, "failed to read jemalloc heap statistics");
            None
        }
    };
    let path = std::env::temp_dir().join(format!(
        "railoxide-{}-{}.heap",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    let path_c = CString::new(path.as_os_str().as_bytes()).wrap_err("encode heap profile path")?;
    let name = c"prof.dump";
    let mut path_ptr = path_c.as_ptr();
    let result = unsafe {
        tikv_jemalloc_sys::mallctl(
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (&raw mut path_ptr).cast::<c_void>(),
            std::mem::size_of::<*const std::ffi::c_char>(),
        )
    };
    if result != 0 {
        eyre::bail!("prof.dump mallctl failed with code {result}");
    }
    Ok((path, stats))
}

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
fn read_jemalloc_stats() -> Result<JemallocStats> {
    let mut epoch = 1_usize;
    let result = unsafe {
        tikv_jemalloc_sys::mallctl(
            c"epoch".as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (&raw mut epoch).cast::<c_void>(),
            std::mem::size_of::<usize>(),
        )
    };
    if result != 0 {
        eyre::bail!("epoch mallctl failed with code {result}");
    }

    Ok(JemallocStats {
        allocated: read_jemalloc_usize(c"stats.allocated")?,
        active: read_jemalloc_usize(c"stats.active")?,
        resident: read_jemalloc_usize(c"stats.resident")?,
        retained: read_jemalloc_usize(c"stats.retained")?,
    })
}

#[cfg(all(target_os = "macos", feature = "heap-profiling"))]
fn read_jemalloc_usize(name: &CStr) -> Result<usize> {
    let mut value = 0_usize;
    let mut value_size = std::mem::size_of::<usize>();
    let result = unsafe {
        tikv_jemalloc_sys::mallctl(
            name.as_ptr(),
            (&raw mut value).cast::<c_void>(),
            &raw mut value_size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        eyre::bail!(
            "{} mallctl failed with code {result}",
            name.to_string_lossy()
        );
    }
    if value_size != std::mem::size_of::<usize>() {
        eyre::bail!(
            "{} mallctl returned {value_size} bytes",
            name.to_string_lossy()
        );
    }
    Ok(value)
}

fn install_tracing(logs: LogStore) -> Result<()> {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    let console_layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_writer(std::io::stderr);
    let ui_layer = UiLogLayer::new(logs);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(ui_layer)
        .try_init()
        .map_err(|error| eyre::eyre!("install tracing subscriber: {error}"))?;

    Ok(())
}

fn install_quit_behavior(app: &mut App) {
    app.on_action(|_: &Quit, cx| cx.quit());
    app.on_window_closed(|cx, _window_id| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();

    #[cfg(target_os = "macos")]
    {
        app.bind_keys([gpui::KeyBinding::new("cmd-q", Quit, None)]);
        app.set_menus(vec![gpui::Menu {
            name: "RailOxide".into(),
            disabled: false,
            items: vec![
                gpui::MenuItem::os_submenu("Services", gpui::SystemMenuType::Services),
                gpui::MenuItem::separator(),
                gpui::MenuItem::action("Quit RailOxide", Quit),
            ],
        }]);
    }
}
