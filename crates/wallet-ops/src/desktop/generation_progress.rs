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
