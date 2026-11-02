use std::fs;
use std::path::Path;

use super::*;

/// The data folder's total size, as shown in Settings Maintenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum DataFolderSize {
    Unknown,
    /// A walk with this generation is running.
    Calculating(u64),
    Ready(u64),
    Missing,
}

/// Sums the sizes of the regular files under `root`. It never follows a symlink inside the
/// folder and skips entries that cannot be read. This blocks, so run it off the UI thread.
pub(in crate::root) fn measure_data_folder(root: &Path) -> DataFolderSize {
    if matches!(root.try_exists(), Ok(false)) {
        return DataFolderSize::Missing;
    }
    let mut total = 0_u64;
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                dirs.push(path);
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    DataFolderSize::Ready(total)
}

/// The path the data folder row shows, copies and opens.
pub(in crate::root) fn data_folder_path(db_path: &Path) -> PathBuf {
    std::path::absolute(db_path).unwrap_or_else(|_| db_path.to_path_buf())
}

/// The data folder path with copy and Open actions, an optional size, and the last Open failure.
pub(in crate::root) fn data_folder_row(
    path: &Path,
    size_label: Option<gpui::Div>,
    exists: bool,
    open_error: Option<SharedString>,
    on_open: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    let path = SharedString::from(path.to_string_lossy().into_owned());
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    app_text(path.clone())
                        .flex_1()
                        .min_w(px(0.0))
                        .font_family(APP_MONO_FONT_FAMILY)
                        .whitespace_normal(),
                )
                .child(clipboard_with_toast("wallet-data-folder-copy", path))
                .child(
                    app_button("wallet-data-folder-open", "Open")
                        .disabled(!exists)
                        .on_click(move |_event, window, cx| on_open(window, cx)),
                ),
        )
        .children(size_label)
        .when_some(open_error, |this, error| {
            this.child(settings_dialog_error(error))
        })
}

/// Asks the system to open `path` in its file manager. `open::that` waits for the opener to
/// exit, so it runs on the background executor; a failure goes to `on_error`.
pub(in crate::root) fn open_data_folder(
    path: PathBuf,
    cx: &App,
    on_error: impl FnOnce(SharedString, &mut App) + 'static,
) {
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { open::that(&path) })
            .await;
        if let Err(error) = result {
            cx.update(|cx| {
                on_error(
                    format!("Could not open the data folder: {error}").into(),
                    cx,
                );
            });
        }
    })
    .detach();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn measure_counts_only_regular_files_inside_the_folder() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), [0_u8; 3]).unwrap();
        fs::create_dir(root.path().join("sub")).unwrap();
        fs::write(root.path().join("sub").join("b"), [0_u8; 5]).unwrap();
        fs::write(outside.path().join("big"), [0_u8; 1000]).unwrap();
        symlink(outside.path().join("big"), root.path().join("link")).unwrap();
        symlink(root.path(), root.path().join("sub").join("loop")).unwrap();

        assert_eq!(measure_data_folder(root.path()), DataFolderSize::Ready(8));
        assert_eq!(
            measure_data_folder(&root.path().join("missing")),
            DataFolderSize::Missing
        );
    }
}
