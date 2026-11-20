#[allow(clippy::wildcard_imports)]
use super::*;

#[must_use]
pub fn filter_public_broadcasters_by_trust(
    candidates: &[PublicBroadcasterCandidate],
    trust_filter: &PublicBroadcasterTrustFilter,
) -> Vec<PublicBroadcasterCandidate> {
    candidates
        .iter()
        .filter(|candidate| trust_filter.allows(candidate))
        .cloned()
        .collect()
}

pub fn eligible_public_broadcasters_for_asset(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
) -> Result<Vec<PublicBroadcasterCandidate>> {
    broadcaster_core::deployment::RailgunDeployment::for_chain(chain_id)
        .ok_or_else(|| eyre!("chain {chain_id} has no Railgun deployment"))?;
    Ok(eligible_public_broadcasters(
        rows,
        chain_id,
        token,
        SystemTime::now(),
    ))
}

pub fn public_broadcaster_candidates_for_asset(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    policy: BroadcasterFeePolicy,
    anchor_rate: Option<U256>,
) -> Result<Vec<PublicBroadcasterCandidate>> {
    broadcaster_core::deployment::RailgunDeployment::for_chain(chain_id)
        .ok_or_else(|| eyre!("chain {chain_id} has no Railgun deployment"))?;
    Ok(public_broadcaster_candidates(
        rows,
        chain_id,
        token,
        SystemTime::now(),
        policy,
        anchor_rate,
    ))
}

#[must_use]
pub fn eligible_public_broadcasters(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    now: SystemTime,
) -> Vec<PublicBroadcasterCandidate> {
    let candidates = rows
        .iter()
        .filter(|row| row.chain_id == chain_id)
        .filter(|row| row.token_address == token)
        .filter(|row| row.fee_expiration > now)
        .filter(|row| row.available_wallets > 0)
        .filter(|row| supported_broadcaster_version(&row.version))
        .filter_map(PublicBroadcasterCandidate::from_fee_row)
        .collect::<Vec<_>>();
    deduplicate_public_broadcasters(candidates)
}

#[must_use]
pub fn public_broadcaster_candidates(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    now: SystemTime,
    policy: BroadcasterFeePolicy,
    anchor_rate: Option<U256>,
) -> Vec<PublicBroadcasterCandidate> {
    let candidates = rows
        .iter()
        .filter(|row| row.chain_id == chain_id)
        .filter(|row| row.token_address == token)
        .filter(|row| row.fee_expiration > now)
        .filter(|row| row.available_wallets > 0)
        .filter(|row| supported_broadcaster_version(&row.version))
        .filter_map(|row| {
            PublicBroadcasterCandidate::from_fee_row_with_policy_status(
                row,
                policy.classify_fee(row.fee, anchor_rate),
            )
        })
        .collect::<Vec<_>>();
    deduplicate_public_broadcasters(candidates)
}

fn deduplicate_public_broadcasters(
    candidates: Vec<PublicBroadcasterCandidate>,
) -> Vec<PublicBroadcasterCandidate> {
    let mut unique: Vec<PublicBroadcasterCandidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if let Some(existing) = unique.iter_mut().find(|existing| {
            existing.chain_id == candidate.chain_id
                && existing.token == candidate.token
                && existing.address_data.master_public_key
                    == candidate.address_data.master_public_key
                && existing.address_data.viewing_public_key
                    == candidate.address_data.viewing_public_key
        }) {
            if public_broadcaster_is_preferred(&candidate, existing) {
                *existing = candidate;
            }
        } else {
            unique.push(candidate);
        }
    }
    unique
}

fn public_broadcaster_is_preferred(
    candidate: &PublicBroadcasterCandidate,
    existing: &PublicBroadcasterCandidate,
) -> bool {
    candidate
        .fee_expiration
        .cmp(&existing.fee_expiration)
        .then_with(|| candidate.fees_id.cmp(&existing.fees_id))
        .then_with(|| candidate.railgun_address.cmp(&existing.railgun_address))
        .then_with(|| candidate.identifier.cmp(&existing.identifier))
        .then_with(|| candidate.fee.cmp(&existing.fee))
        .then_with(|| {
            candidate
                .required_poi_list_keys
                .cmp(&existing.required_poi_list_keys)
        })
        .then_with(|| candidate.version.cmp(&existing.version))
        .then_with(|| candidate.reliability.total_cmp(&existing.reliability))
        .then_with(|| candidate.available_wallets.cmp(&existing.available_wallets))
        .then_with(|| candidate.relay_adapt.cmp(&existing.relay_adapt))
        .then_with(|| candidate.relay_adapt_7702.cmp(&existing.relay_adapt_7702))
        .is_gt()
}

#[must_use]
pub(crate) fn random_eligible_public_broadcasters(
    candidates: &[PublicBroadcasterCandidate],
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
) -> Vec<PublicBroadcasterCandidate> {
    let eligible = candidates
        .iter()
        .filter(|candidate| trust_filter.allows(candidate))
        .filter(|candidate| candidate.is_allowed_by_fee_policy(policy))
        .filter(|candidate| candidate.parsed_required_poi_list_keys().is_ok())
        .cloned()
        .collect();
    deduplicate_public_broadcasters(eligible)
}

#[must_use]
pub fn fee_policy_eligible_public_broadcasters(
    candidates: &[PublicBroadcasterCandidate],
    policy: BroadcasterFeePolicy,
) -> Vec<PublicBroadcasterCandidate> {
    candidates
        .iter()
        .filter(|candidate| candidate.is_allowed_by_fee_policy(policy))
        .cloned()
        .collect()
}

#[must_use]
pub fn sort_specific_public_broadcasters(
    mut candidates: Vec<PublicBroadcasterCandidate>,
    sort_seed: &[u8; 32],
) -> Vec<PublicBroadcasterCandidate> {
    candidates.sort_by(|a, b| {
        a.fee
            .cmp(&b.fee)
            .then_with(|| {
                seeded_broadcaster_sort_key(sort_seed, &a.railgun_address)
                    .cmp(&seeded_broadcaster_sort_key(sort_seed, &b.railgun_address))
            })
            .then_with(|| a.railgun_address.cmp(&b.railgun_address))
    });
    candidates
}

fn seeded_broadcaster_sort_key(sort_seed: &[u8; 32], railgun_address: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(sort_seed);
    hasher.update(railgun_address.as_bytes());
    hasher.finalize().into()
}

pub fn select_public_broadcaster(
    candidates: &[PublicBroadcasterCandidate],
    selection: &PublicBroadcasterSelection,
) -> Result<PublicBroadcasterCandidate> {
    select_public_broadcaster_with_policy(
        candidates,
        selection,
        BroadcasterFeePolicy::default().with_allow_suspicious_broadcasters(true),
    )
}

pub fn select_public_broadcaster_with_policy(
    candidates: &[PublicBroadcasterCandidate],
    selection: &PublicBroadcasterSelection,
    policy: BroadcasterFeePolicy,
) -> Result<PublicBroadcasterCandidate> {
    select_public_broadcaster_with_policy_and_trust(
        candidates,
        selection,
        policy,
        &PublicBroadcasterTrustFilter::default(),
    )
}

pub fn select_public_broadcaster_with_policy_and_trust(
    candidates: &[PublicBroadcasterCandidate],
    selection: &PublicBroadcasterSelection,
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
) -> Result<PublicBroadcasterCandidate> {
    match selection {
        PublicBroadcasterSelection::Random => {
            let eligible_candidates =
                random_eligible_public_broadcasters(candidates, policy, trust_filter);
            let selected = eligible_candidates.choose(&mut rand::rng()).cloned();
            selected.ok_or_else(|| eyre!("no eligible public broadcaster for selected token"))
        }
        PublicBroadcasterSelection::Specific { railgun_address } => {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.railgun_address == *railgun_address)
                .cloned()
                .ok_or_else(|| eyre!("selected public broadcaster is no longer eligible"))?;
            if !trust_filter.allows(&candidate) {
                return Err(eyre!(
                    "selected public broadcaster is excluded by current preferences"
                ));
            }
            if candidate.is_allowed_by_fee_policy(policy) {
                Ok(candidate)
            } else {
                Err(eyre!(
                    "selected public broadcaster fee is outside the allowed range"
                ))
            }
        }
    }
}
