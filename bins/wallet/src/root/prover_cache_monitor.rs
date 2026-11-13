use std::time::Duration;

use gpui::Context;
use wallet_ops::{ProverCacheBuildProgress, subscribe_prover_cache_build};

use super::WalletRoot;

const PROVER_CACHE_BUILD_DISCOVERY_INTERVAL: Duration = Duration::from_secs(1);

impl WalletRoot {
    pub(super) const fn is_prover_cache_building(&self) -> bool {
        self.prover_cache_build_progress.is_some()
    }

    pub(super) fn ensure_prover_cache_build_monitor(&mut self, cx: &Context<'_, Self>) {
        if self.prover_cache_build_monitor_active {
            return;
        }
        let Some(db_path) = self.wallet_db_root_dir() else {
            return;
        };
        self.prover_cache_build_monitor_active = true;
        cx.spawn(async move |this, cx| {
            loop {
                if let Some(mut progress_rx) = subscribe_prover_cache_build(&db_path) {
                    let progress = progress_rx.borrow().clone();
                    if this
                        .update(cx, |root, cx| {
                            root.set_prover_cache_build_progress(progress, cx);
                        })
                        .is_err()
                    {
                        break;
                    }

                    loop {
                        if progress_rx.changed().await.is_err() {
                            let _ = this.update(cx, |root, cx| {
                                root.set_prover_cache_build_progress(None, cx);
                            });
                            break;
                        }
                        let progress = progress_rx.borrow().clone();
                        let is_complete = progress.is_none();
                        if this
                            .update(cx, |root, cx| {
                                root.set_prover_cache_build_progress(progress, cx);
                            })
                            .is_err()
                        {
                            return;
                        }
                        if is_complete {
                            break;
                        }
                    }
                }

                cx.background_executor()
                    .timer(PROVER_CACHE_BUILD_DISCOVERY_INTERVAL)
                    .await;
            }
        })
        .detach();
    }

    fn set_prover_cache_build_progress(
        &mut self,
        progress: Option<ProverCacheBuildProgress>,
        cx: &mut Context<'_, Self>,
    ) {
        self.prover_cache_build_progress = progress;
        if self.prover_cache_build_progress.is_none() {
            self.prover_cache_build_popover_open = false;
        }
        cx.notify();
    }

    pub(super) fn update_prover_cache_build_progress(
        &mut self,
        progress: ProverCacheBuildProgress,
        cx: &mut Context<'_, Self>,
    ) {
        self.prover_cache_build_progress = Some(progress);
        cx.notify();
    }

    pub(super) fn finish_prover_cache_build_progress(&mut self, cx: &mut Context<'_, Self>) {
        self.prover_cache_build_progress = None;
        self.prover_cache_build_popover_open = false;
        cx.notify();
    }

    pub(super) fn set_prover_cache_build_popover_open(
        &mut self,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if self.prover_cache_build_popover_open != open {
            self.prover_cache_build_popover_open = open;
            cx.notify();
        }
    }
}
