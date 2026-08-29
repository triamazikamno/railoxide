use super::{
    PublicAccountSource, PublicActionGasRetryKind, PublicActionMode, PublicActionProgressStep,
    PublicActionStepState, PublicActionStepStatus, PublicAssetId, STOPPED_PROGRESS_MESSAGE,
    SharedString,
};

pub(in crate::root) const fn public_action_step_label(
    step: PublicActionProgressStep,
) -> &'static str {
    match step {
        PublicActionProgressStep::ShieldKey => "Authorize shield key",
        PublicActionProgressStep::Send => "Send",
        PublicActionProgressStep::Wrap => "Wrap",
        PublicActionProgressStep::Approve => "Approve",
        PublicActionProgressStep::Shield => "Shield",
        PublicActionProgressStep::Sponsor => "Sponsor proposal",
        PublicActionProgressStep::Unsponsor => "Unsponsor proposal",
        PublicActionProgressStep::CallVote => "Call vote",
        PublicActionProgressStep::Vote => "Vote",
        PublicActionProgressStep::GovernanceApprove => "Approve governance token",
        PublicActionProgressStep::Stake => "Stake",
        PublicActionProgressStep::Delegate => "Delegate",
        PublicActionProgressStep::Undelegate => "Undelegate",
        PublicActionProgressStep::Unlock => "Unlock",
        PublicActionProgressStep::PrincipalClaim => "Claim principal",
        PublicActionProgressStep::RewardClaim(_) => "Claim rewards",
    }
}

#[cfg(test)]
pub(in crate::root) const fn public_action_step_detail(
    step: PublicActionProgressStep,
    status: PublicActionStepStatus,
) -> &'static str {
    public_action_step_detail_for_context(step, status, false, false)
}

pub(in crate::root) const fn public_action_step_detail_for_context(
    step: PublicActionProgressStep,
    status: PublicActionStepStatus,
    requires_device_approval: bool,
    tx_submitted: bool,
) -> &'static str {
    match status {
        PublicActionStepStatus::NotStarted => match step {
            PublicActionProgressStep::ShieldKey => "Waiting to authorize the shield key message.",
            PublicActionProgressStep::Send => "Waiting to broadcast the transfer.",
            PublicActionProgressStep::Wrap => "Waiting to wrap the native token.",
            PublicActionProgressStep::Approve => "Waiting to approve the shield contract.",
            PublicActionProgressStep::Shield => "Waiting to shield into the Private wallet.",
            PublicActionProgressStep::Sponsor => "Waiting to sponsor the proposal.",
            PublicActionProgressStep::Unsponsor => "Waiting to unsponsor the proposal.",
            PublicActionProgressStep::CallVote => "Waiting to call the vote.",
            PublicActionProgressStep::Vote => "Waiting to submit the vote.",
            PublicActionProgressStep::GovernanceApprove => {
                "Waiting to approve the governance token allowance."
            }
            PublicActionProgressStep::Stake => "Waiting to stake governance tokens.",
            PublicActionProgressStep::Delegate => "Waiting to delegate the stake.",
            PublicActionProgressStep::Undelegate => "Waiting to undelegate the stake.",
            PublicActionProgressStep::Unlock => "Waiting to unlock the stake.",
            PublicActionProgressStep::PrincipalClaim => "Waiting to claim the staked principal.",
            PublicActionProgressStep::RewardClaim(_) => "Waiting to claim rewards.",
        },
        PublicActionStepStatus::Pending => match (step, requires_device_approval, tx_submitted) {
            (PublicActionProgressStep::ShieldKey, _, _) => {
                "Approve the RAILGUN_SHIELD message on your hardware wallet."
            }
            (_, _, true) => "Broadcasted and waiting for confirmation.",
            (_, true, false) => {
                "Approve the transaction on your hardware wallet, then wait for broadcast."
            }
            (_, false, false) => "Broadcasting and waiting for confirmation.",
        },
        PublicActionStepStatus::Done => "Confirmed on-chain.",
        PublicActionStepStatus::Error => "Failed.",
        PublicActionStepStatus::Stopped => STOPPED_PROGRESS_MESSAGE,
    }
}

pub(in crate::root) fn public_action_error_summary(
    step: PublicActionProgressStep,
    details: Option<&str>,
    asset_label: &str,
) -> String {
    let details = details.unwrap_or_default().to_ascii_lowercase();
    if details.contains("cancel") || details.contains("rejected") || details.contains("denied") {
        return match step {
            PublicActionProgressStep::ShieldKey => {
                "Shield key authorization was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Send => {
                "Public send signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Wrap => {
                format!("Wrapping {asset_label} was cancelled on the hardware wallet.")
            }
            PublicActionProgressStep::Approve => {
                "Approval signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Shield => {
                "Shield transaction signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Sponsor => {
                "Sponsorship signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Unsponsor => {
                "Unsponsorship signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::CallVote => {
                "Call-vote signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Vote => {
                "Vote signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::GovernanceApprove => {
                "Governance approval signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Stake => {
                "Stake signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Delegate => {
                "Delegation signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Undelegate => {
                "Undelegation signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::Unlock => {
                "Unlock signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::PrincipalClaim => {
                "Principal claim signing was cancelled on the hardware wallet.".to_string()
            }
            PublicActionProgressStep::RewardClaim(_) => {
                "Reward claim signing was cancelled on the hardware wallet.".to_string()
            }
        };
    }
    if details.contains("estimate gas") {
        return match step {
            PublicActionProgressStep::ShieldKey => {
                "Could not prepare hardware shield key authorization.".to_string()
            }
            PublicActionProgressStep::Send => {
                "Could not estimate gas. Check amount, recipient, and gas balance.".to_string()
            }
            PublicActionProgressStep::Wrap => format!(
                "Could not estimate gas to wrap {asset_label}. Check amount and gas balance."
            ),
            PublicActionProgressStep::Approve => {
                "Could not estimate gas for approval. Check token balance and try again."
                    .to_string()
            }
            PublicActionProgressStep::Shield => {
                "Could not estimate gas for shielding. Try again or check the RPC/network."
                    .to_string()
            }
            PublicActionProgressStep::Sponsor => {
                "Could not estimate gas for sponsorship. Check proposal state and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::Unsponsor => {
                "Could not estimate gas for unsponsorship. Check proposal state and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::CallVote => {
                "Could not estimate gas for calling the vote. Check proposal state and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::Vote => {
                "Could not estimate gas for voting. Check voting power and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::GovernanceApprove => {
                "Could not estimate gas for governance approval. Check token balance and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::Stake => {
                "Could not estimate gas for staking. Check token balance and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::Delegate => {
                "Could not estimate gas for delegation. Check stake and gas balance.".to_string()
            }
            PublicActionProgressStep::Undelegate => {
                "Could not estimate gas for undelegation. Check stake and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::Unlock => {
                "Could not estimate gas for unlocking. Check stake state and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::PrincipalClaim => {
                "Could not estimate gas for claiming principal. Check stake state and gas balance."
                    .to_string()
            }
            PublicActionProgressStep::RewardClaim(_) => {
                "Could not estimate gas for claiming rewards. Check reward state and gas balance."
                    .to_string()
            }
        };
    }
    if details.contains("revert") {
        return match step {
            PublicActionProgressStep::ShieldKey => {
                "Shield key authorization failed before broadcast.".to_string()
            }
            PublicActionProgressStep::Send => "Transfer reverted on-chain.".to_string(),
            PublicActionProgressStep::Wrap => format!("Wrapping {asset_label} reverted on-chain."),
            PublicActionProgressStep::Approve => "Approval reverted on-chain.".to_string(),
            PublicActionProgressStep::Shield => "Shielding reverted on-chain.".to_string(),
            PublicActionProgressStep::Sponsor => "Sponsorship reverted on-chain.".to_string(),
            PublicActionProgressStep::Unsponsor => "Unsponsorship reverted on-chain.".to_string(),
            PublicActionProgressStep::CallVote => "Calling the vote reverted on-chain.".to_string(),
            PublicActionProgressStep::Vote => "Voting reverted on-chain.".to_string(),
            PublicActionProgressStep::GovernanceApprove => {
                "Governance approval reverted on-chain.".to_string()
            }
            PublicActionProgressStep::Stake => "Staking reverted on-chain.".to_string(),
            PublicActionProgressStep::Delegate => "Delegation reverted on-chain.".to_string(),
            PublicActionProgressStep::Undelegate => "Undelegation reverted on-chain.".to_string(),
            PublicActionProgressStep::Unlock => "Unlocking reverted on-chain.".to_string(),
            PublicActionProgressStep::PrincipalClaim => {
                "Principal claim reverted on-chain.".to_string()
            }
            PublicActionProgressStep::RewardClaim(_) => {
                "Reward claim reverted on-chain.".to_string()
            }
        };
    }
    match step {
        PublicActionProgressStep::ShieldKey => {
            "Could not authorize the shield key on the hardware wallet.".to_string()
        }
        PublicActionProgressStep::Send => {
            "Could not send publicly. Check amount, recipient, and gas balance.".to_string()
        }
        PublicActionProgressStep::Wrap => {
            format!("Could not wrap {asset_label}. Check amount and gas balance.")
        }
        PublicActionProgressStep::Approve => {
            "Could not approve the shield contract. Check token balance and try again.".to_string()
        }
        PublicActionProgressStep::Shield => {
            "Could not shield into the Private wallet. Try again or check the RPC/network."
                .to_string()
        }
        PublicActionProgressStep::Sponsor => {
            "Could not sponsor the proposal. Check proposal state, voting power, and gas balance."
                .to_string()
        }
        PublicActionProgressStep::Unsponsor => {
            "Could not unsponsor the proposal. Check proposal state and gas balance.".to_string()
        }
        PublicActionProgressStep::CallVote => {
            "Could not call the vote. Check proposal state and gas balance.".to_string()
        }
        PublicActionProgressStep::Vote => {
            "Could not submit the vote. Check voting power and gas balance.".to_string()
        }
        PublicActionProgressStep::GovernanceApprove => {
            "Could not approve governance tokens. Check token balance and gas balance.".to_string()
        }
        PublicActionProgressStep::Stake => {
            "Could not stake governance tokens. Check token balance and gas balance.".to_string()
        }
        PublicActionProgressStep::Delegate => {
            "Could not delegate the stake. Check stake and gas balance.".to_string()
        }
        PublicActionProgressStep::Undelegate => {
            "Could not undelegate the stake. Check stake and gas balance.".to_string()
        }
        PublicActionProgressStep::Unlock => {
            "Could not unlock the stake. Check stake state and gas balance.".to_string()
        }
        PublicActionProgressStep::PrincipalClaim => {
            "Could not claim staked principal. Check stake state and gas balance.".to_string()
        }
        PublicActionProgressStep::RewardClaim(_) => {
            "Could not claim rewards. Try again or check the RPC/network.".to_string()
        }
    }
}

pub(in crate::root) fn public_action_error_details(
    summary: &str,
    details: Option<&str>,
) -> Option<String> {
    let details = details?.trim();
    if details.is_empty() || details == summary {
        None
    } else {
        Some(details.to_string())
    }
}

pub(in crate::root) fn public_action_error_copy_value(
    step: PublicActionProgressStep,
    asset_label: &str,
    summary: &str,
    details: Option<&str>,
) -> String {
    let mut value = format!(
        "Step: {}\nAsset: {asset_label}\nSummary: {summary}",
        public_action_step_label(step),
    );
    if let Some(details) = details {
        value.push_str("\nDetails: ");
        value.push_str(details);
    }
    value
}

pub(in crate::root) fn public_action_step_id(step: PublicActionProgressStep) -> String {
    match step {
        PublicActionProgressStep::ShieldKey => "shield-key".to_string(),
        PublicActionProgressStep::Send => "send".to_string(),
        PublicActionProgressStep::Wrap => "wrap".to_string(),
        PublicActionProgressStep::Approve => "approve".to_string(),
        PublicActionProgressStep::Shield => "shield".to_string(),
        PublicActionProgressStep::Sponsor => "sponsor".to_string(),
        PublicActionProgressStep::Unsponsor => "unsponsor".to_string(),
        PublicActionProgressStep::CallVote => "call-vote".to_string(),
        PublicActionProgressStep::Vote => "vote".to_string(),
        PublicActionProgressStep::GovernanceApprove => "governance-approve".to_string(),
        PublicActionProgressStep::Stake => "stake".to_string(),
        PublicActionProgressStep::Delegate => "delegate".to_string(),
        PublicActionProgressStep::Undelegate => "undelegate".to_string(),
        PublicActionProgressStep::Unlock => "unlock".to_string(),
        PublicActionProgressStep::PrincipalClaim => "principal-claim".to_string(),
        PublicActionProgressStep::RewardClaim(index) => format!("reward-claim-{index}"),
    }
}

pub(in crate::root) fn public_action_retry_button_id(
    step: PublicActionProgressStep,
    retry_kind: PublicActionGasRetryKind,
) -> SharedString {
    let action = match retry_kind {
        PublicActionGasRetryKind::RetryStep => "retry-step",
        PublicActionGasRetryKind::RetryEstimate => "retry-gas",
        PublicActionGasRetryKind::SpeedUp => "speed-up",
        PublicActionGasRetryKind::FeeAuthorization => "authorize-fee",
    };
    SharedString::from(format!(
        "wallet-public-action-{}-{action}",
        public_action_step_id(step)
    ))
}

pub(in crate::root) fn public_action_progress_steps(
    mode: PublicActionMode,
    asset: PublicAssetId,
) -> Vec<PublicActionProgressStep> {
    public_action_progress_steps_for_source(mode, asset, PublicAccountSource::Derived)
}

pub(in crate::root) fn public_action_progress_steps_for_source(
    mode: PublicActionMode,
    asset: PublicAssetId,
    source: PublicAccountSource,
) -> Vec<PublicActionProgressStep> {
    let mut steps = Vec::new();
    if mode == PublicActionMode::Shield && source == PublicAccountSource::HardwareDerived {
        steps.push(PublicActionProgressStep::ShieldKey);
    }
    match mode {
        PublicActionMode::Send => steps.push(PublicActionProgressStep::Send),
        PublicActionMode::Shield if asset == PublicAssetId::Native => {
            steps.push(PublicActionProgressStep::Shield);
        }
        PublicActionMode::Shield => {
            steps.push(PublicActionProgressStep::Approve);
            steps.push(PublicActionProgressStep::Shield);
        }
    }
    steps
}

pub(in crate::root) fn public_action_error_retry_kind(
    step: &PublicActionStepState,
) -> PublicActionGasRetryKind {
    let message = step
        .message
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if message.contains("estimate gas")
        || message.contains("insufficient native gas")
        || message.contains("insufficient native balance")
    {
        PublicActionGasRetryKind::RetryEstimate
    } else {
        PublicActionGasRetryKind::RetryStep
    }
}
