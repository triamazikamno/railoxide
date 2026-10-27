use tokio::sync::watch;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TransactionGenerationStage {
    #[default]
    SelectingPrivateNotes,
    ProvingTransaction,
    EstimatingBroadcasterFee,
    GeneratingPoiProofs,
    PublishingToBroadcaster,
    WaitingForBroadcasterResponse,
    EstimatingSelfBroadcastGas,
    SigningSelfBroadcast,
    WaitingForSelfBroadcastReceipt,
}

impl TransactionGenerationStage {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SelectingPrivateNotes => "Selecting private notes",
            Self::ProvingTransaction => "Proving transaction",
            Self::EstimatingBroadcasterFee => "Estimating transaction fee",
            Self::GeneratingPoiProofs => "Generating POI proofs",
            Self::PublishingToBroadcaster => "Publishing to broadcaster",
            Self::WaitingForBroadcasterResponse => "Waiting for broadcaster response",
            Self::EstimatingSelfBroadcastGas => "Estimating self-broadcast gas",
            Self::SigningSelfBroadcast => "Signing self-broadcast transaction",
            Self::WaitingForSelfBroadcastReceipt => "Waiting for self-broadcast receipt",
        }
    }

    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::SelectingPrivateNotes => {
                "Finding POI-verified notes that cover the amount and fee."
            }
            Self::ProvingTransaction => {
                "Generating the zero-knowledge proof. This is usually the slowest step."
            }
            Self::EstimatingBroadcasterFee => "Checking gas cost and transaction fee requirements.",
            Self::GeneratingPoiProofs => "Generating POI proofs for transaction outputs.",
            Self::PublishingToBroadcaster => "Encrypting and publishing the request over Waku.",
            Self::WaitingForBroadcasterResponse => {
                "Waiting for the selected broadcaster to respond."
            }
            Self::EstimatingSelfBroadcastGas => {
                "Estimating direct transaction gas and checking the gas payer balance."
            }
            Self::SigningSelfBroadcast => "Unlocking the selected Public account and signing.",
            Self::WaitingForSelfBroadcastReceipt => {
                "Waiting for the submitted transaction receipt."
            }
        }
    }
}

pub type TransactionGenerationProgressSender = watch::Sender<TransactionGenerationStage>;

pub(super) fn update_transaction_generation_stage(
    progress_tx: Option<&TransactionGenerationProgressSender>,
    stage: TransactionGenerationStage,
) {
    if let Some(progress_tx) = progress_tx {
        let _ = progress_tx.send(stage);
    }
}

/// Where a public broadcaster submission's own Waku route stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubmissionRouteState {
    /// The submission hasn't started its node yet.
    #[default]
    Preparing,
    /// The node is dialing its first peers.
    Connecting,
    /// The node has a `LightPush` peer and a filter peer.
    Ready,
    /// The node lost its route and is reconnecting.
    Reconnecting,
    /// The submission stopped using its route. Its node may still be retiring.
    Closed,
}

/// A submission's route, for its progress view. A submission that ends, however it ends,
/// leaves its route `Closed` with its counts kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubmissionRouteProgress {
    pub state: SubmissionRouteState,
    /// The node's connected Waku peers.
    pub peers: usize,
    /// Route recoveries started, out of `SUBMISSION_ROUTE_RECOVERY_LIMIT`.
    pub recoveries_used: u32,
    /// Publish attempts made while the route was ready, the first one included.
    pub publishes: u32,
}

pub type SubmissionRouteSender = watch::Sender<SubmissionRouteProgress>;

pub(super) fn update_submission_route(
    route_tx: Option<&SubmissionRouteSender>,
    update: impl FnOnce(&mut SubmissionRouteProgress),
) {
    if let Some(route_tx) = route_tx {
        route_tx.send_if_modified(|route| {
            let before = *route;
            update(route);
            *route != before
        });
    }
}

/// Closes a submission's route however the submission ends, including when its future is
/// dropped. Only the state changes, so the counts stay readable.
pub(super) struct ClosingSubmissionRoute(pub(super) Option<SubmissionRouteSender>);

impl Drop for ClosingSubmissionRoute {
    fn drop(&mut self) {
        update_submission_route(self.0.as_ref(), |route| {
            route.state = SubmissionRouteState::Closed;
        });
    }
}
