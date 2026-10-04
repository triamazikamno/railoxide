use super::{
    Address, B256, BlockNumHash, Deserialize, ExecutorAsset, ExecutorDerivationScheme,
    ExecutorNonceObservation, ExecutorOperationId, ExecutorPayloadPurpose, ExecutorRecord,
    ExecutorRecordOrigin, ExecutorStore, ExecutorStoreError, ExecutorUseCheck, FixedBytes,
    IssuedExecutorPayload, IssuedExecutorRecoveryTransaction, LEGACY_VERSION,
    SWAP_DESTINATION_PURPOSE_SUMMARY, Serialize, SwapAccountRole, SwapAccountUse,
    SwapAdmissionEvidence, SwapApproval, SwapApprovedAccount, SwapDelivery, SwapDestinationOutcome,
    SwapDestinationRecord, SwapOperationRecord, SwapOrderRecord, VERSION, local_timestamp,
    swap_account_refusal,
};

/// One logical swap: its review, account pair, setup choices and order attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SwapUseId(FixedBytes<16>);

impl SwapUseId {
    pub fn random() -> Result<Self, ExecutorStoreError> {
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|_| ExecutorStoreError::Unavailable)?;
        Ok(Self(id.into()))
    }

    #[must_use]
    pub fn opaque_id(self) -> String {
        alloy::hex::encode(self.0)
    }

    /// The first use of a source account takes that account's operation identity. It is also
    /// what records from before uses project to, so both chains agree without coordination.
    #[must_use]
    pub const fn first(origin: ExecutorOperationId) -> Self {
        Self(origin.0)
    }
}

/// One account's part in one swap use. Uses stay in the record as history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapUseRecord {
    pub(super) id: SwapUseId,
    #[serde(default)]
    pub(super) started_at: Option<u64>,
    /// This use reserved the account fresh, so it needs setup. False for explicit reuse.
    #[serde(default)]
    pub(super) fresh: bool,
    #[serde(default)]
    pub(super) stopped: bool,
    pub(super) role: SwapUseRole,
    /// The matched source was stopped before it persisted any order of this use.
    #[serde(default)]
    pub(super) stopped_before_order: bool,
}

impl SwapUseRecord {
    #[must_use]
    pub const fn id(&self) -> SwapUseId {
        self.id
    }
    /// Local start time, in Unix seconds.
    #[must_use]
    pub const fn started_at(&self) -> Option<u64> {
        self.started_at
    }
    #[must_use]
    pub const fn is_fresh(&self) -> bool {
        self.fresh
    }
    #[must_use]
    pub const fn is_stopped(&self) -> bool {
        self.stopped
    }
    #[must_use]
    pub const fn role(&self) -> &SwapUseRole {
        &self.role
    }
    /// The approval saved with this source use. Destination uses have no approval.
    #[must_use]
    pub fn approval(&self) -> Option<&SwapApproval> {
        match &self.role {
            SwapUseRole::Source { approval, .. } => approval.as_deref(),
            SwapUseRole::Destination { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapUseRole {
    /// The account places the swap's orders.
    Source {
        /// The terms approved before setup, while no order records its own.
        #[serde(default)]
        approval: Option<Box<SwapApproval>>,
        /// A private Bridge swap's destination stealth account's operation on the destination
        /// chain.
        #[serde(default)]
        destination_operation: Option<ExecutorOperationId>,
    },
    /// The account receives a private Bridge swap's delivery from another chain.
    Destination {
        origin_chain: u64,
        origin_operation: ExecutorOperationId,
        destination_token: Address,
        /// Hashes of the `SwapDestinationShield` payloads issued for this use.
        #[serde(default)]
        shields: Vec<B256>,
        /// What became of the shield payload, from the origin swap's bridge outcome.
        #[serde(default)]
        outcome: Option<SwapDestinationOutcome>,
    },
}

/// One side of a swap's account choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAccountChoice {
    /// Allocate a fresh account under this new operation identity.
    New(ExecutorOperationId),
    /// Reuse the account this operation identifies.
    Existing(ExecutorOperationId),
}

impl SwapAccountChoice {
    #[must_use]
    pub const fn operation(self) -> ExecutorOperationId {
        match self {
            Self::New(operation) | Self::Existing(operation) => operation,
        }
    }
}

/// The destination stealth account of a private Bridge swap's claim.
#[derive(Debug, Clone)]
pub struct SwapDestinationClaim {
    pub chain_id: u64,
    pub account: SwapAccountChoice,
    /// The destination chain's accepted delegate, for a new account.
    pub delegate: Address,
    pub destination_token: Address,
}

/// The accounts one swap use claims together, before any of its preparation.
#[derive(Debug, Clone)]
pub struct SwapPairClaim {
    pub id: SwapUseId,
    pub source: SwapAccountChoice,
    /// This chain's accepted delegate, for a new source account.
    pub delegate: Address,
    /// The purpose summary of a new source account.
    pub purpose_summary: Option<String>,
    /// The assets of a new source account.
    pub assets: Vec<ExecutorAsset>,
    /// The approved terms saved with the source use.
    pub approval: SwapApproval,
    /// Present exactly for a private Bridge delivery.
    pub destination: Option<SwapDestinationClaim>,
}

/// The records of a claimed pair, each holding the claim's use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedSwapPair {
    pub source: ExecutorRecord,
    pub destination: Option<ExecutorRecord>,
}

/// What cancelling a swap use left of its claim on one account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapUseRelease {
    /// Nothing the account issued is unresolved. An existing account is free for another use;
    /// a fresh one stays stopped and is never allocated again.
    Released,
    /// A payload the account issued can still execute. Its nonce and input guards stay, and
    /// the account stays unavailable until canonical reconciliation resolves it.
    IssuedWorkRemains,
}

/// The outcome of [`ExecutorStore::cancel_swap_use`] for each account of the use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapUseCancellation {
    pub source: SwapUseRelease,
    /// `None` when the use claimed no destination account.
    pub destination: Option<SwapUseRelease>,
}

impl ExecutorRecord {
    /// Every swap use of this account, in the order they started.
    #[must_use]
    pub fn swap_uses(&self) -> &[SwapUseRecord] {
        &self.swap_uses
    }
    /// The swap use that currently claims this account.
    #[must_use]
    pub const fn active_swap_use(&self) -> Option<SwapUseId> {
        self.active_swap_use
    }
    #[must_use]
    pub fn swap_use(&self, id: SwapUseId) -> Option<&SwapUseRecord> {
        self.swap_uses.iter().find(|swap_use| swap_use.id == id)
    }

    /// What the latest use serves, when it is a destination use.
    #[must_use]
    pub const fn swap_destination(&self) -> Option<SwapDestinationRecord> {
        match self.swap_uses.as_slice().last() {
            Some(swap_use) => destination_view(swap_use),
            None => None,
        }
    }
    /// What the use `id` serves, when it is a destination use of this account. Reads an
    /// earlier swap's delivery after the account took a later use.
    #[must_use]
    pub fn swap_destination_use(&self, id: SwapUseId) -> Option<SwapDestinationRecord> {
        self.swap_use(id).and_then(destination_view)
    }
    /// The destination account's operation the latest use names, when it is a source use.
    #[must_use]
    pub const fn destination_operation(&self) -> Option<ExecutorOperationId> {
        match self.swap_uses.as_slice().last() {
            Some(SwapUseRecord {
                role:
                    SwapUseRole::Source {
                        destination_operation,
                        ..
                    },
                ..
            }) => *destination_operation,
            _ => None,
        }
    }

    pub(super) fn active_use(&self) -> Option<&SwapUseRecord> {
        self.swap_use(self.active_swap_use?)
    }

    pub(super) fn swap_use_mut(&mut self, id: SwapUseId) -> Option<&mut SwapUseRecord> {
        self.swap_uses.iter_mut().find(|swap_use| swap_use.id == id)
    }

    /// Orders of the use `id`, from oldest to newest.
    pub fn swap_use_orders(
        &self,
        id: SwapUseId,
    ) -> impl DoubleEndedIterator<Item = &SwapOrderRecord> {
        self.swap
            .iter()
            .flat_map(SwapOperationRecord::orders)
            .filter(move |order| order.use_id() == Some(id))
    }

    /// Whether an order of this record belongs to the use `id`.
    #[must_use]
    pub fn has_swap_use_order(&self, id: SwapUseId) -> bool {
        self.swap_use_orders(id).next().is_some()
    }

    /// Whether the destination shield `hash` was signed for a use whose fill ran its shield.
    pub(super) fn swap_shield_delivered(&self, hash: B256) -> bool {
        self.swap_uses.iter().any(|swap_use| {
            matches!(
                &swap_use.role,
                SwapUseRole::Destination {
                    shields,
                    outcome: Some(SwapDestinationOutcome::Shielded { .. }),
                    ..
                } if shields.contains(&hash)
            )
        })
    }

    /// Whether this origin record's use `id` is a source use naming `destination`.
    pub(super) fn links_swap_destination(
        &self,
        id: SwapUseId,
        destination: ExecutorOperationId,
    ) -> bool {
        self.swap_use(id).is_some_and(|swap_use| {
            matches!(
                swap_use.role,
                SwapUseRole::Source {
                    destination_operation: Some(linked),
                    ..
                } if linked == destination
            )
        })
    }

    /// The chain and operation of the destination account the source use `id` names. The chain
    /// is the one its approval's private Bridge delivery binds, or one of its orders'.
    fn swap_use_destination(&self, id: SwapUseId) -> Option<(u64, ExecutorOperationId)> {
        let SwapUseRole::Source {
            approval,
            destination_operation,
        } = &self.swap_use(id)?.role
        else {
            return None;
        };
        let destination = (*destination_operation)?;
        let private_chain = |delivery: SwapDelivery| {
            delivery
                .private_bridge()
                .map(|bridge| bridge.destination_chain)
        };
        let chain = approval
            .as_deref()
            .and_then(|approval| private_chain(approval.delivery))
            .or_else(|| {
                self.swap_use_orders(id)
                    .find_map(|order| private_chain(order.delivery()))
            })?;
        Some((chain, destination))
    }

    /// Whether this record's use `id` is a destination use of the swap `origin_operation` on
    /// `origin_chain`.
    pub(crate) fn serves_swap_use(
        &self,
        id: SwapUseId,
        origin_chain: u64,
        origin_operation: ExecutorOperationId,
    ) -> bool {
        self.swap_use(id).is_some_and(|swap_use| {
            matches!(
                swap_use.role,
                SwapUseRole::Destination {
                    origin_chain: chain,
                    origin_operation: operation,
                    ..
                } if chain == origin_chain && operation == origin_operation
            )
        })
    }

    /// Stop the use `id`, which has no order, and release what it holds of this account. An
    /// account the use reserved fresh takes the stop policy of an abandoned setup, and a fresh
    /// destination is retired as reconciliation retires it. An existing account is neither
    /// retired nor stopped: it is released unless the use issued a shield, which keeps its
    /// guards until fresh admission resolves it. Issued payloads and inputs stay as they are.
    pub(super) fn cancel_swap_use(&mut self, id: SwapUseId) -> SwapUseRelease {
        let Some(swap_use) = self.swap_use_mut(id) else {
            return SwapUseRelease::Released;
        };
        swap_use.stopped = true;
        swap_use.stopped_before_order = true;
        let fresh = swap_use.fresh;
        let (destination, shielded) = match &swap_use.role {
            SwapUseRole::Destination { shields, .. } => (true, !shields.is_empty()),
            SwapUseRole::Source { .. } => (false, false),
        };
        if fresh && self.active_swap_use == Some(id) {
            self.swap_setup_stopped = true;
            self.retired |= destination;
            return if self.has_recorded_unresolved_issued_work() {
                SwapUseRelease::IssuedWorkRemains
            } else {
                SwapUseRelease::Released
            };
        }
        if shielded {
            return SwapUseRelease::IssuedWorkRemains;
        }
        if self.active_swap_use == Some(id) {
            self.active_swap_use = None;
        }
        SwapUseRelease::Released
    }

    /// The use an account takes with its reservation, which is what a record from before uses
    /// projects to: fresh, active, and started when the account was reserved.
    pub(super) fn begin_first_swap_use(&mut self, id: SwapUseId, role: SwapUseRole) {
        self.swap_uses.push(SwapUseRecord {
            id,
            started_at: self.created_at,
            fresh: true,
            stopped: false,
            role,
            stopped_before_order: false,
        });
        self.active_swap_use = Some(id);
    }

    /// A use an existing account takes after its reservation. The account keeps its setup, so
    /// the use is not fresh. The caller checks `admits_swap_use` first.
    fn begin_later_swap_use(&mut self, id: SwapUseId, role: SwapUseRole) {
        if let SwapUseRole::Destination {
            destination_token, ..
        } = &role
        {
            let asset = ExecutorAsset::Erc20(*destination_token);
            if !self.assets.contains(&asset) {
                self.assets.push(asset);
            }
        }
        self.swap_uses.push(SwapUseRecord {
            id,
            started_at: local_timestamp(),
            fresh: false,
            stopped: false,
            role,
            stopped_before_order: false,
        });
        self.active_swap_use = Some(id);
    }

    /// The stored format follows the content: a record without use metadata keeps the shape
    /// builds from before uses read.
    pub(super) fn format_version(&self) -> u32 {
        if self.swap_uses.is_empty()
            && self.active_swap_use.is_none()
            && !self
                .swap
                .as_ref()
                .is_some_and(SwapOperationRecord::has_use_ids)
        {
            LEGACY_VERSION
        } else {
            VERSION
        }
    }
}

const fn destination_view(swap_use: &SwapUseRecord) -> Option<SwapDestinationRecord> {
    match &swap_use.role {
        SwapUseRole::Destination {
            origin_chain,
            origin_operation,
            destination_token,
            outcome,
            ..
        } => Some(SwapDestinationRecord {
            origin_chain: *origin_chain,
            origin_operation: *origin_operation,
            destination_token: *destination_token,
            outcome: *outcome,
        }),
        SwapUseRole::Source { .. } => None,
    }
}

/// The stored shape of an [`ExecutorRecord`]. Version 1 holds one swap's links in
/// `swap_approval`, `swap_destination` and `destination_operation`. Version 2 leaves those
/// empty and holds them in `swap_uses`.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ExecutorRecordWire {
    version: u32,
    derivation: ExecutorDerivationScheme,
    origin: ExecutorRecordOrigin,
    operation: ExecutorOperationId,
    index: u32,
    address: Option<Address>,
    delegate: Address,
    retired: bool,
    created_at: Option<u64>,
    restored_at: Option<u64>,
    purpose_summary: Option<String>,
    #[serde(default)]
    assets: Vec<ExecutorAsset>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    use_check: ExecutorUseCheck,
    issued: Vec<IssuedExecutorPayload>,
    #[serde(default)]
    nonce_observation: Option<ExecutorNonceObservation>,
    #[serde(default)]
    recovery_transactions: Vec<IssuedExecutorRecoveryTransaction>,
    #[serde(default)]
    recovery_observation: Option<BlockNumHash>,
    #[serde(default)]
    public_account_uuid: Option<String>,
    #[serde(default)]
    swap: Option<SwapOperationRecord>,
    #[serde(default)]
    swap_approval: Option<SwapApproval>,
    #[serde(default)]
    swap_setup_stopped: bool,
    #[serde(default)]
    released_payloads: Vec<B256>,
    #[serde(default)]
    swap_destination: Option<SwapDestinationRecord>,
    #[serde(default)]
    destination_operation: Option<ExecutorOperationId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    swap_uses: Vec<SwapUseRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_swap_use: Option<SwapUseId>,
}

impl TryFrom<ExecutorRecordWire> for ExecutorRecord {
    type Error = &'static str;

    /// Projects a version-1 record's links into uses, in memory only. The same stored record
    /// always projects to the same uses.
    fn try_from(wire: ExecutorRecordWire) -> Result<Self, Self::Error> {
        let ExecutorRecordWire {
            version,
            derivation,
            origin,
            operation,
            index,
            address,
            delegate,
            retired,
            created_at,
            restored_at,
            purpose_summary,
            assets,
            hidden,
            use_check,
            issued,
            nonce_observation,
            recovery_transactions,
            recovery_observation,
            public_account_uuid,
            mut swap,
            swap_approval,
            swap_setup_stopped,
            released_payloads,
            swap_destination,
            destination_operation,
            mut swap_uses,
            mut active_swap_use,
        } = wire;
        let has_use_metadata = !swap_uses.is_empty()
            || active_swap_use.is_some()
            || swap.as_ref().is_some_and(SwapOperationRecord::has_use_ids);
        let has_legacy_links = swap_approval.is_some()
            || swap_destination.is_some()
            || destination_operation.is_some();
        if version == LEGACY_VERSION && has_use_metadata {
            return Err("version 1 executor record carries swap uses");
        }
        if version == VERSION && has_legacy_links {
            return Err("version 2 executor record carries version 1 swap links");
        }
        if version == LEGACY_VERSION {
            let first_use = |id, role| SwapUseRecord {
                id,
                started_at: created_at,
                fresh: true,
                stopped: swap_setup_stopped,
                role,
                stopped_before_order: false,
            };
            // Both kinds of link on one record conflict. Keep both, the source use last.
            if let Some(destination) = swap_destination {
                swap_uses.push(first_use(
                    SwapUseId::first(destination.origin_operation),
                    SwapUseRole::Destination {
                        origin_chain: destination.origin_chain,
                        origin_operation: destination.origin_operation,
                        destination_token: destination.destination_token,
                        shields: issued
                            .iter()
                            .filter(|payload| {
                                payload.purpose == ExecutorPayloadPurpose::SwapDestinationShield
                            })
                            .map(|payload| payload.hash)
                            .collect(),
                        outcome: destination.outcome,
                    },
                ));
            }
            if swap.is_some() || swap_approval.is_some() || destination_operation.is_some() {
                let id = SwapUseId::first(operation);
                if let Some(swap) = &mut swap {
                    swap.assign_use(id);
                }
                swap_uses.push(first_use(
                    id,
                    SwapUseRole::Source {
                        approval: swap_approval.map(Box::new),
                        destination_operation,
                    },
                ));
            }
            active_swap_use = swap_uses.last().map(|swap_use| swap_use.id);
        }
        let mut record = Self {
            version,
            derivation,
            origin,
            operation,
            index,
            address,
            delegate,
            retired,
            created_at,
            restored_at,
            purpose_summary,
            assets,
            hidden,
            use_check,
            issued,
            nonce_observation,
            recovery_transactions,
            recovery_observation,
            public_account_uuid,
            swap,
            swap_setup_stopped,
            released_payloads,
            swap_uses,
            active_swap_use,
        };
        // An unsupported version stays as stored, for the store to reject.
        if (LEGACY_VERSION..=VERSION).contains(&version) {
            record.version = record.format_version();
        }
        Ok(record)
    }
}

impl From<ExecutorRecord> for ExecutorRecordWire {
    fn from(record: ExecutorRecord) -> Self {
        let version = record.format_version();
        let ExecutorRecord {
            version: _,
            derivation,
            origin,
            operation,
            index,
            address,
            delegate,
            retired,
            created_at,
            restored_at,
            purpose_summary,
            assets,
            hidden,
            use_check,
            issued,
            nonce_observation,
            recovery_transactions,
            recovery_observation,
            public_account_uuid,
            swap,
            swap_setup_stopped,
            released_payloads,
            swap_uses,
            active_swap_use,
        } = record;
        Self {
            version,
            derivation,
            origin,
            operation,
            index,
            address,
            delegate,
            retired,
            created_at,
            restored_at,
            purpose_summary,
            assets,
            hidden,
            use_check,
            issued,
            nonce_observation,
            recovery_transactions,
            recovery_observation,
            public_account_uuid,
            swap,
            swap_approval: None,
            swap_setup_stopped,
            released_payloads,
            swap_destination: None,
            destination_operation: None,
            swap_uses,
            active_swap_use,
        }
    }
}

/// Whether an existing account on `chain_id` can take the new swap use `id` in `role`, from
/// its recorded outcomes. The shared admission rules decide; the store reports every refusal
/// as a claim it can't grant.
fn admits_swap_use(
    record: &ExecutorRecord,
    chain_id: u64,
    role: SwapAccountRole,
    id: SwapUseId,
) -> bool {
    record.swap_use(id).is_none()
        && swap_account_refusal(
            record,
            chain_id,
            role,
            SwapAccountUse::New,
            SwapAdmissionEvidence::Recorded,
        )
        .is_none()
}

/// Whether one side of a claim can take the use `id` in `role`: a new account's operation
/// identity is unused, and an existing account on `chain_id` is there and admits another use.
fn admit_swap_account(
    choice: SwapAccountChoice,
    record: Option<&ExecutorRecord>,
    chain_id: u64,
    role: SwapAccountRole,
    id: SwapUseId,
) -> Result<(), ExecutorStoreError> {
    match (choice, record) {
        (SwapAccountChoice::New(_), None) => Ok(()),
        (SwapAccountChoice::Existing(_), Some(record)) => {
            if admits_swap_use(record, chain_id, role, id) {
                Ok(())
            } else {
                Err(ExecutorStoreError::SwapUseActive)
            }
        }
        (SwapAccountChoice::New(_), Some(_)) | (SwapAccountChoice::Existing(_), None) => {
            Err(ExecutorStoreError::OperationMismatch)
        }
    }
}

impl ExecutorStore {
    /// Claim a swap use's accounts together, on the source chain's store: the source account
    /// here and, for a private Bridge delivery, the destination account on its chain. Both
    /// sides are checked before either is written, and the records and allocation rows of both
    /// chains commit in one write, so a refused claim leaves nothing on either account. A claim
    /// both accounts already hold is returned unchanged.
    pub fn claim_swap_pair(
        &self,
        claim: SwapPairClaim,
    ) -> Result<ClaimedSwapPair, ExecutorStoreError> {
        let SwapPairClaim {
            id,
            source,
            delegate,
            purpose_summary,
            assets,
            approval,
            destination,
        } = claim;
        // A destination is on another chain than its swap.
        if destination
            .as_ref()
            .is_some_and(|destination| destination.chain_id == self.chain_id)
        {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        let _guard = super::EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let source_operation = source.operation();
        let destination =
            destination.map(|destination| (self.for_chain(destination.chain_id), destination));
        let destination_operation = destination
            .as_ref()
            .map(|(_, destination)| destination.account.operation());
        let source_record = self.record(source_operation)?;
        let destination_record = match &destination {
            Some((store, destination)) => store.record(destination.account.operation())?,
            None => None,
        };

        if let Some(accounts) = approval.accounts {
            let admits = |approved: SwapApprovedAccount,
                          choice: SwapAccountChoice,
                          record: Option<&ExecutorRecord>| {
                let setup = matches!(choice, SwapAccountChoice::New(_));
                // An approved new address is checked after derivation. Existing addresses,
                // setup choices and destination presence are known before any claim writes.
                record
                    .and_then(ExecutorRecord::address)
                    .map_or(approved.setup == setup, |address| {
                        approved.admits(address, setup)
                    })
            };
            let destination_admitted = match (accounts.destination, destination.as_ref()) {
                (None, None) => true,
                (Some(approved), Some((_, destination))) => {
                    admits(approved, destination.account, destination_record.as_ref())
                }
                _ => false,
            };
            if !admits(accounts.source, source, source_record.as_ref()) || !destination_admitted {
                return Err(ExecutorStoreError::OperationMismatch);
            }
        }

        let source_use = source_record
            .as_ref()
            .and_then(|record| record.swap_use(id));
        let destination_use = destination_record
            .as_ref()
            .and_then(|record| record.swap_use(id));
        if source_use.is_some() || destination_use.is_some() {
            let source_bound = source_use.is_some_and(|swap_use| {
                matches!(
                    swap_use.role,
                    SwapUseRole::Source { destination_operation: linked, .. }
                        if linked == destination_operation
                )
            });
            let destination_bound = destination.as_ref().is_none_or(|(_, destination)| {
                destination_use.is_some_and(|swap_use| {
                    matches!(
                        swap_use.role,
                        SwapUseRole::Destination {
                            origin_chain,
                            origin_operation,
                            destination_token,
                            ..
                        } if origin_chain == self.chain_id
                            && origin_operation == source_operation
                            && destination_token == destination.destination_token
                    )
                })
            });
            return match source_record {
                Some(source) if source_bound && destination_bound => Ok(ClaimedSwapPair {
                    source,
                    destination: destination_record,
                }),
                _ => Err(ExecutorStoreError::OperationMismatch),
            };
        }

        admit_swap_account(
            source,
            source_record.as_ref(),
            self.chain_id,
            SwapAccountRole::Source,
            id,
        )?;
        if let Some((_, destination)) = &destination {
            admit_swap_account(
                destination.account,
                destination_record.as_ref(),
                destination.chain_id,
                SwapAccountRole::Destination {
                    token: destination.destination_token,
                },
                id,
            )?;
        }

        let mut updates = Vec::new();
        let source = self.claim_swap_account(
            source_operation,
            source_record,
            (delegate, purpose_summary.as_deref(), assets.as_slice()),
            (
                id,
                SwapUseRole::Source {
                    approval: Some(Box::new(approval)),
                    destination_operation,
                },
            ),
            &mut updates,
        )?;
        let destination = destination
            .map(|(store, destination)| {
                let received = [ExecutorAsset::Erc20(destination.destination_token)];
                store.claim_swap_account(
                    destination.account.operation(),
                    destination_record,
                    (
                        destination.delegate,
                        Some(SWAP_DESTINATION_PURPOSE_SUMMARY),
                        received.as_slice(),
                    ),
                    (
                        id,
                        SwapUseRole::Destination {
                            origin_chain: self.chain_id,
                            origin_operation: source_operation,
                            destination_token: destination.destination_token,
                            shields: Vec::new(),
                            outcome: None,
                        },
                    ),
                    &mut updates,
                )
            })
            .transpose()?;
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(ClaimedSwapPair {
            source,
            destination,
        })
    }

    /// Cancel the swap use `id` before it has an order, on the source chain's store. The use is
    /// stopped on its source account here and on its destination account, and each account is
    /// released as [`SwapUseRelease`] reports, in one write across both chains. A use with an
    /// order is refused: its orders end through their own early cancellation.
    pub fn cancel_swap_use(
        &self,
        source_operation: ExecutorOperationId,
        id: SwapUseId,
    ) -> Result<SwapUseCancellation, ExecutorStoreError> {
        let _guard = super::EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut source = self
            .record(source_operation)?
            .ok_or(ExecutorStoreError::OperationMismatch)?;
        let Some(SwapUseRecord {
            role:
                SwapUseRole::Source {
                    destination_operation,
                    ..
                },
            ..
        }) = source.swap_use(id)
        else {
            return Err(ExecutorStoreError::OperationMismatch);
        };
        if source.has_swap_use_order(id) {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        // Both accounts are read before either is written.
        let destination = match *destination_operation {
            Some(operation) => {
                let (chain_id, _) = source
                    .swap_use_destination(id)
                    .ok_or(ExecutorStoreError::OperationMismatch)?;
                let store = self.for_chain(chain_id);
                let record = store
                    .record(operation)?
                    .filter(|record| record.serves_swap_use(id, self.chain_id, source_operation))
                    .ok_or(ExecutorStoreError::OperationMismatch)?;
                Some((store, record))
            }
            None => None,
        };
        let mut updates = Vec::new();
        let released = source.cancel_swap_use(id);
        updates.push(self.seal(
            super::RecordKind::ExecutorOperation,
            self.operation_key(source_operation),
            &source,
        )?);
        let destination = destination
            .map(|(store, mut record)| {
                let released = record.cancel_swap_use(id);
                updates.push(store.seal(
                    super::RecordKind::ExecutorOperation,
                    store.operation_key(record.operation),
                    &record,
                )?);
                Ok::<_, ExecutorStoreError>(released)
            })
            .transpose()?;
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(SwapUseCancellation {
            source: released,
            destination,
        })
    }

    /// Stop issuance on both accounts of the active use. An orderless source releases each
    /// account through the same transition as cancellation; issued work and its guards stay.
    /// Repeating the stop also repairs an already-stopped orderless claim.
    pub(crate) fn stop_swap_use(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let _guard = super::EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut record = self
            .record(operation)?
            .ok_or(ExecutorStoreError::OperationMismatch)?;
        if record.active_use().is_none() {
            return Ok(record);
        }
        let mut updates = Vec::new();
        self.stop_swap_use_in_batch(&mut record, &mut updates)?;
        updates.push(self.seal(
            super::RecordKind::ExecutorOperation,
            self.operation_key(operation),
            &record,
        )?);
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(record)
    }

    /// Prepare the active use's stop while the caller holds `EXECUTOR_RECORD_LOCK`. The caller
    /// seals `record` with its other changes and commits it with the linked record in `updates`.
    pub(super) fn stop_swap_use_in_batch(
        &self,
        record: &mut ExecutorRecord,
        updates: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<(), ExecutorStoreError> {
        let Some(active) = record.active_use() else {
            return Ok(());
        };
        let id = active.id;
        let source = matches!(active.role, SwapUseRole::Source { .. });
        let counterpart = match &active.role {
            SwapUseRole::Source { .. } => record.swap_use_destination(id),
            SwapUseRole::Destination {
                origin_chain,
                origin_operation,
                ..
            } => Some((*origin_chain, *origin_operation)),
        };
        let mut other = counterpart
            .map(|(chain_id, operation)| {
                let store = self.for_chain(chain_id);
                let other = store.record(operation)?.filter(|other| {
                    if source {
                        other.serves_swap_use(id, self.chain_id, record.operation)
                    } else {
                        other.links_swap_destination(id, record.operation)
                    }
                });
                Ok::<_, ExecutorStoreError>((store, other))
            })
            .transpose()?;
        let orderless = if source {
            !record.has_swap_use_order(id)
        } else {
            other.as_ref().is_some_and(|(_, other)| {
                other
                    .as_ref()
                    .is_some_and(|origin| !origin.has_swap_use_order(id))
            })
        };
        if orderless {
            record.cancel_swap_use(id);
        } else if let Some(active) = record.swap_use_mut(id) {
            active.stopped = true;
        }
        if let Some((store, Some(other))) = &mut other {
            if orderless {
                other.cancel_swap_use(id);
            } else if let Some(linked_use) = other.swap_use_mut(id) {
                linked_use.stopped = true;
            }
            updates.push(store.seal(
                super::RecordKind::ExecutorOperation,
                store.operation_key(other.operation),
                other,
            )?);
        }
        Ok(())
    }

    /// Give one admitted side of a claim its use and append the result to `updates` without
    /// writing: the existing `record` with the use added, or a fresh account allocated for
    /// `operation` on this chain with the delegate, purpose summary and assets in `fresh`.
    fn claim_swap_account(
        &self,
        operation: ExecutorOperationId,
        record: Option<ExecutorRecord>,
        fresh: (Address, Option<&str>, &[ExecutorAsset]),
        (id, role): (SwapUseId, SwapUseRole),
        updates: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let Some(mut record) = record else {
            let (delegate, purpose_summary, assets) = fresh;
            return self.allocate_record(
                operation,
                delegate,
                purpose_summary,
                assets,
                Some((id, role)),
                updates,
            );
        };
        record.begin_later_swap_use(id, role);
        record.version = record.format_version();
        updates.push(self.seal(
            super::RecordKind::ExecutorOperation,
            self.operation_key(operation),
            &record,
        )?);
        Ok(record)
    }

    /// List the way a build from before swap uses does: it accepts version 1 only, and one
    /// other record fails the whole chain's list.
    #[cfg(test)]
    pub(crate) fn records_as_version_1_reader(
        &self,
    ) -> Result<Vec<ExecutorRecord>, ExecutorStoreError> {
        #[derive(Deserialize)]
        struct StoredVersion {
            version: u32,
        }
        for stored in
            self.vault
                .db
                .list_desktop_wallet_vault_records(&super::executor_operation_prefix(
                    self.view.wallet_id(),
                    self.chain_id,
                ))?
        {
            let StoredVersion { version } = self.open(
                super::RecordKind::ExecutorOperation,
                &stored.key,
                &stored.payload,
            )?;
            if version != LEGACY_VERSION {
                return Err(ExecutorStoreError::InvalidRecord);
            }
        }
        self.records()
    }

    /// Store `value` as the record of `operation`, as an earlier build wrote it.
    #[cfg(test)]
    pub(crate) fn put_operation_fixture<T: Serialize>(
        &self,
        operation: ExecutorOperationId,
        value: &T,
    ) -> Result<(), ExecutorStoreError> {
        let _guard = super::EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.vault.db.put_desktop_wallet_vault_records(&[self.seal(
            super::RecordKind::ExecutorOperation,
            self.operation_key(operation),
            value,
        )?])?;
        Ok(())
    }
}
