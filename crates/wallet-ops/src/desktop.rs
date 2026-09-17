use super::*;

mod local_cache;
mod private_tx;
mod prover_cache;
mod public_broadcaster;
mod public_broadcaster_submit;
mod requests;
mod self_broadcast;
mod sessions;
mod sync_helpers;

pub async fn initialize_created_wallet_chain_metadata_for_session(
    view_session: Arc<vault::DesktopViewSession>,
    effective_chains: BTreeMap<u64, settings::EffectiveChainConfig>,
    db: Arc<DbStore>,
    http: HttpContext,
    skip_chain_id: Option<u64>,
    init_policy: CreatedWalletChainInitPolicy,
) {
    let report = initialize_new_wallet_chain_metadata_for_session(
        view_session,
        effective_chains,
        db,
        http,
        skip_chain_id,
        init_policy,
    )
    .await;
    tracing::info!(
        initialized = report.initialized,
        skipped_disabled = report.skipped_disabled,
        skipped_unavailable = report.skipped_unavailable,
        skipped_selected = report.skipped_selected,
        skipped_existing = report.skipped_existing,
        failed = report.failed,
        "new wallet chain metadata initialization complete"
    );
}

pub use local_cache::*;
pub use private_tx::*;
pub use prover_cache::*;
#[cfg(test)]
pub(crate) use public_broadcaster::random_eligible_public_broadcasters;
pub use public_broadcaster::*;
pub(crate) use public_broadcaster_submit::*;
pub use requests::*;
pub use self_broadcast::SelfBroadcastResolvedGasFee;
pub(crate) use self_broadcast::*;
pub use self_broadcast::{
    SponsoredSelfBroadcastCommand, SponsoredSelfBroadcastCommandSender,
    SponsoredSelfBroadcastSessionOutcome, SponsoredSelfBroadcastSessionRequest,
    SponsoredSelfBroadcastStopReason, run_sponsored_self_broadcast_session,
    sponsored_self_broadcast_head_poll_interval,
};
pub use sessions::*;
pub use sync_helpers::fetch_current_safe_head;
pub(crate) use sync_helpers::*;
