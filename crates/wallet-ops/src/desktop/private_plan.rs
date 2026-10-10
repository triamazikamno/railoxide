use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedUnshieldCall {
    pub chain_id: u64,
    pub token: Address,
    pub amount: U256,
    pub fee_mode: FeeHandlingMode,
    pub recipient: Address,
    pub unwrap: bool,
    pub max_spendable: U256,
    pub transaction_count: usize,
    pub input_count: usize,
    pub private_output_count: usize,
    pub public_output_count: usize,
    pub to: Address,
    pub data: String,
    pub native_top_up: Option<DesktopNativeTopUpPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSendCall {
    pub chain_id: u64,
    pub token: Address,
    pub amount: U256,
    pub recipient: String,
    pub max_spendable: U256,
    pub transaction_count: usize,
    pub input_count: usize,
    pub private_output_count: usize,
    pub public_output_count: usize,
    pub to: Address,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSponsoredCall {
    pub chain_id: u64,
    pub action: SponsoredActionKind,
    pub authorization: SponsoredAuthorization,
    pub transaction_count: usize,
    pub input_count: usize,
    pub private_output_count: usize,
    pub public_output_count: usize,
    pub relay_call_count: usize,
    pub uses_relay_adapt: bool,
    pub selected_inputs: Vec<SelectedInputIdentity>,
    pub native_top_up: Option<DesktopNativeTopUpPlan>,
    pub total_wrapped_native_spend: U256,
    pub to: Address,
    pub data: String,
}

pub(super) struct PreparedPrivatePlan<P> {
    pub(super) plan: P,
    pub(super) max_spendable: U256,
    pub(super) prover: ProverService,
}

pub(super) struct PreparedDesktopUnshieldPlan {
    pub(super) transaction: Option<TransactionRequest>,
    pub(super) plan: DesktopUnshieldPreparedPlan,
    pub(super) max_spendable: U256,
    pub(super) prover: ProverService,
    pub(super) native_top_up: Option<DesktopNativeTopUpPlan>,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(super) enum DesktopUnshieldPreparedPlan {
    Single(UnshieldPlan),
    Composite(CompositeUnshieldPlan),
}

impl DesktopUnshieldPreparedPlan {
    pub(super) fn chunks(&self) -> &[TransactionPlanChunk] {
        match self {
            Self::Single(plan) => &plan.chunks,
            Self::Composite(plan) => &plan.chunks,
        }
    }

    pub(super) fn input_utxos(&self) -> Vec<Utxo> {
        match self {
            Self::Single(plan) => plan.inputs.iter().map(|input| input.utxo.clone()).collect(),
            Self::Composite(plan) => plan.inputs.iter().map(|input| input.utxo.clone()).collect(),
        }
    }

    pub(super) const fn call_to(&self) -> Address {
        match self {
            Self::Single(plan) => plan.call.to,
            Self::Composite(plan) => plan.call.to,
        }
    }

    pub(super) fn call_data(&self) -> Bytes {
        match self {
            Self::Single(plan) => plan.call.data.clone(),
            Self::Composite(plan) => plan.call.data.clone(),
        }
    }

    pub(super) const fn transaction_count(&self) -> usize {
        match self {
            Self::Single(plan) => plan.transaction_count(),
            Self::Composite(plan) => plan.shape.transaction_count,
        }
    }

    pub(super) const fn input_count(&self) -> usize {
        match self {
            Self::Single(plan) => plan.input_count(),
            Self::Composite(plan) => plan.shape.input_count,
        }
    }

    pub(super) const fn private_output_count(&self) -> usize {
        match self {
            Self::Single(plan) => plan.private_output_count(),
            Self::Composite(plan) => plan.shape.private_output_count,
        }
    }

    pub(super) const fn public_output_count(&self) -> usize {
        match self {
            Self::Single(plan) => plan.public_output_count(),
            Self::Composite(plan) => plan.shape.public_output_count,
        }
    }
}

pub(super) struct DesktopUnshieldPlanRequest<'a> {
    pub(super) executor: Option<&'a PreparedExecutorOperation>,
    pub(super) chain_id: u64,
    pub(super) effective_chain: &'a settings::EffectiveChainConfig,
    pub(super) view_session: &'a vault::DesktopViewSession,
    pub(super) session: &'a WalletSession,
    pub(super) vault_store: &'a vault::DesktopVaultStore,
    pub(super) spend_authorization: &'a DesktopPrivateSpendAuthorization,
    pub(super) token: Address,
    pub(super) amount: U256,
    pub(super) fee_mode: FeeHandlingMode,
    pub(super) recipient: Address,
    pub(super) unwrap: bool,
    pub(super) native_top_up: Option<DesktopNativeTopUpRequest>,
    pub(super) verify_proof: bool,
    pub(super) progress_tx: Option<&'a TransactionGenerationProgressSender>,
}

pub(super) struct DesktopSendPlanRequest<'a> {
    pub(super) chain_id: u64,
    pub(super) effective_chain: &'a settings::EffectiveChainConfig,
    pub(super) view_session: &'a vault::DesktopViewSession,
    pub(super) session: &'a WalletSession,
    pub(super) vault_store: &'a vault::DesktopVaultStore,
    pub(super) spend_authorization: &'a DesktopPrivateSpendAuthorization,
    pub(super) token: Address,
    pub(super) amount: U256,
    pub(super) recipient: &'a str,
    pub(super) verify_proof: bool,
    pub(super) progress_tx: Option<&'a TransactionGenerationProgressSender>,
}
