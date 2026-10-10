//! The signed approval of an order paid from a Public account: an EIP-2612 permit of the sold
//! token to `CoW`'s vault relayer, which the order's pre-hook submits in place of an approval
//! transaction.
//!
//! Whether a token has such a permit is decided from the token's own answers: the wallet signs
//! only under a typed-data domain that reproduces the `DOMAIN_SEPARATOR()` the token reports.
//! The reads travel with the allowance read, and what they found is kept for the session. The
//! signed permit is then called on the token in a standalone `eth_call` before the order
//! leaves the wallet.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::MutexGuard;

use alloy::primitives::{Address, B256, Bytes, Signature, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{Eip712Domain, SolCall as _, SolStruct as _};
use broadcaster_core::contracts::cow::{AppDataHook, OrderUid};
use broadcaster_core::contracts::erc20_permit::{IERC20Permit, Permit, permit_calldata};
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, WrapErr as _, eyre};
use serde_json::{Value, json};

use super::observation::SwapSettlement;
use super::public_order::{NO_PUBLIC_SWAPS, typed_data};
use super::simulation::{Execution, execute};
use crate::public_wallet::{PublicErc20, VaultedPublicSigner, sign_public_swap_typed_data};
use crate::settings::EffectiveChainConfig;
use crate::vault::{ExecutorOperationId, PublicSwapPermit, PublicSwapRecord, SwapUseId};
use crate::{
    ExecutorOwner, RpcBrokerError, RpcRoute, WalletRpcOrigin, query_rpc_pool_with_http_client,
};

const ALLOWANCE_READ: &str = "read the Public account's allowance for the swap";
const PERMIT_UNUSABLE: &str = "the token's permit couldn't be used; review the swap again";

/// The answer to one read of a token.
type Read = Result<Bytes, RpcBrokerError>;

/// How a Public account lets a swap's spender take the amount it sells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicSwapApprovalPlan {
    /// The allowance covers the amount: nothing is signed or sent.
    None,
    /// The account signs a permit that the order's pre-hook submits, and sends nothing.
    Permit(PublicSwapPermitPlan),
    /// The approvals the account sends, in order: the exact approval when the allowance is
    /// zero, and a reset to zero first when it is short and not zero. Tokens such as
    /// Ethereum's USDT reject a change from one nonzero allowance to another, so the reset
    /// applies to every token.
    Transactions(Vec<U256>),
}

/// What a permit is signed with: the token's confirmed typed-data domain and the account's
/// current permit nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicSwapPermitPlan {
    domain: Eip712Domain,
    nonce: U256,
}

#[cfg(feature = "test-support")]
impl PublicSwapPermitPlan {
    /// A plan under an empty domain at nonce zero.
    pub(super) const fn for_tests() -> Self {
        Self {
            domain: Eip712Domain::new(None, None, None, None, None),
            nonce: U256::ZERO,
        }
    }
}

impl PublicSwapApprovalPlan {
    /// The plan for selling `amount` with `allowance`, where `permit` is what the sold token's
    /// permit would be signed with. Only an order's token has one.
    #[must_use]
    pub(crate) fn new(allowance: U256, amount: U256, permit: Option<PublicSwapPermitPlan>) -> Self {
        if allowance >= amount {
            Self::None
        } else if let Some(permit) = permit {
            Self::Permit(permit)
        } else if allowance.is_zero() {
            Self::Transactions(vec![amount])
        } else {
            Self::Transactions(vec![U256::ZERO, amount])
        }
    }

    /// The value of each approval transaction the account sends, in order.
    #[must_use]
    pub fn transactions(&self) -> &[U256] {
        match self {
            Self::Transactions(values) => values,
            Self::None | Self::Permit(_) => &[],
        }
    }

    #[must_use]
    pub const fn signs_permit(&self) -> bool {
        matches!(self, Self::Permit(_))
    }
}

/// What a session found of a token's permit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermitSupport {
    /// The typed-data domain that reproduces the token's domain separator.
    Domain(Eip712Domain),
    /// The token answered and has no permit the wallet can sign.
    Absent,
}

/// The reads that decide a token's permit, as [`detection_calls`] orders them.
pub(super) struct PermitReads {
    pub(super) eip712_domain: Read,
    pub(super) name: Read,
    pub(super) version: Read,
    pub(super) domain_separator: Read,
    pub(super) nonce: Read,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PermitDetection {
    Confirmed {
        domain: Eip712Domain,
        nonce: U256,
    },
    /// The token answered and no domain the wallet can build is its own.
    Absent,
    /// A read the decision needs failed. Nothing is known of the token.
    Unknown,
}

/// What one read says of the token.
enum Answer<T> {
    Value(T),
    /// The call reverted, or returned data that doesn't decode.
    Refused,
    /// The read itself failed: its transport, its deadline or its endpoint.
    Failed,
}

fn answer<T>(read: &Read, decode: impl FnOnce(&[u8]) -> Option<T>) -> Answer<T> {
    match read {
        Ok(output) => decode(output).map_or(Answer::Refused, Answer::Value),
        Err(RpcBrokerError::InnerRevert(_)) => Answer::Refused,
        Err(_) => Answer::Failed,
    }
}

/// The calls [`detect_permit`] reads, for `owner`'s nonce on `token`.
fn detection_calls(token: Address, owner: Address) -> [(Address, Bytes); 5] {
    [
        IERC20Permit::eip712DomainCall {}.abi_encode(),
        IERC20Permit::nameCall {}.abi_encode(),
        IERC20Permit::versionCall {}.abi_encode(),
        IERC20Permit::DOMAIN_SEPARATORCall {}.abi_encode(),
        IERC20Permit::noncesCall { owner }.abi_encode(),
    ]
    .map(|call| (token, call.into()))
}

/// The domain `eip712Domain()` reported, built from exactly the fields its bitmap names.
/// `None` for a domain with extensions or with fields EIP-5267 doesn't define, which the
/// wallet can't sign under, and for one that isn't bound to `token` on `chain_id`: a token can
/// report another token's domain, and a permit signed under it is valid there. A bound domain
/// names `token` as its verifying contract and the chain as its chain id, or without a chain
/// id as its salt.
fn reported_domain(
    chain_id: u64,
    token: Address,
    reported: &IERC20Permit::eip712DomainReturn,
) -> Option<Eip712Domain> {
    let fields = reported.fields[0];
    if !reported.extensions.is_empty() || fields > 0x1f {
        return None;
    }
    let has = |bit: u8| fields & bit != 0;
    let chain = U256::from(chain_id);
    if !has(0x08) || reported.verifyingContract != token {
        return None;
    }
    let bound_to_chain = if has(0x04) {
        reported.chainId == chain
    } else {
        has(0x10) && reported.salt == B256::from(chain)
    };
    if !bound_to_chain {
        return None;
    }
    Some(Eip712Domain::new(
        has(0x01).then(|| Cow::Owned(reported.name.clone())),
        has(0x02).then(|| Cow::Owned(reported.version.clone())),
        has(0x04).then_some(reported.chainId),
        has(0x08).then_some(reported.verifyingContract),
        has(0x10).then_some(reported.salt),
    ))
}

/// Pure: whether `token` on `chain_id` has a permit the wallet can sign, from its own answers.
///
/// The candidates are the domain `eip712Domain()` reports when it names this token and chain,
/// then `name()` with `version()`, or "1" for a token without one, in the token's own domain
/// with the chain id, and with the chain id as a `salt` instead, as bridged tokens on some
/// chains use. The first whose hash is the reported `DOMAIN_SEPARATOR()` is confirmed, with
/// the account's nonce. A token that reports no separator or no nonce, or whose separator no
/// candidate reproduces, has no permit. A reverting `eip712Domain()` or `version()` is an
/// answer, not a failure.
pub(super) fn detect_permit(chain_id: u64, token: Address, reads: &PermitReads) -> PermitDetection {
    let separator = answer(&reads.domain_separator, |output| {
        IERC20Permit::DOMAIN_SEPARATORCall::abi_decode_returns(output).ok()
    });
    let nonce = answer(&reads.nonce, |output| {
        IERC20Permit::noncesCall::abi_decode_returns(output).ok()
    });
    let (separator, nonce) = match (separator, nonce) {
        (Answer::Refused, _) | (_, Answer::Refused) => return PermitDetection::Absent,
        (Answer::Value(separator), Answer::Value(nonce)) => (separator, nonce),
        _ => return PermitDetection::Unknown,
    };
    let mut failed = false;
    let mut candidates = Vec::new();
    match answer(&reads.eip712_domain, |output| {
        IERC20Permit::eip712DomainCall::abi_decode_returns(output).ok()
    }) {
        Answer::Value(reported) => {
            candidates.extend(reported_domain(chain_id, token, &reported));
        }
        Answer::Refused => {}
        Answer::Failed => failed = true,
    }
    let name = match answer(&reads.name, |output| {
        IERC20Permit::nameCall::abi_decode_returns(output).ok()
    }) {
        Answer::Value(name) => Some(name),
        Answer::Refused => None,
        Answer::Failed => {
            failed = true;
            None
        }
    };
    if let Some(name) = name {
        let version = match answer(&reads.version, |output| {
            IERC20Permit::versionCall::abi_decode_returns(output).ok()
        }) {
            Answer::Value(version) => version,
            Answer::Refused => "1".to_owned(),
            Answer::Failed => {
                failed = true;
                "1".to_owned()
            }
        };
        let (name, version) = (Some(Cow::Owned(name)), Some(Cow::Owned(version)));
        candidates.push(Eip712Domain::new(
            name.clone(),
            version.clone(),
            Some(U256::from(chain_id)),
            Some(token),
            None,
        ));
        candidates.push(Eip712Domain::new(
            name,
            version,
            None,
            Some(token),
            Some(B256::from(U256::from(chain_id))),
        ));
    }
    match candidates
        .into_iter()
        .find(|domain| domain.separator() == separator)
    {
        Some(domain) => PermitDetection::Confirmed { domain, nonce },
        None if failed => PermitDetection::Unknown,
        None => PermitDetection::Absent,
    }
}

/// [`detect_permit`] from the next five of `results`, the answers to [`detection_calls`].
fn detect_permit_from(
    chain_id: u64,
    token: Address,
    results: &mut impl Iterator<Item = Read>,
) -> Result<PermitDetection> {
    let mut read = || {
        results
            .next()
            .ok_or_else(|| eyre!("a permit read returned nothing"))
    };
    let reads = PermitReads {
        eip712_domain: read()?,
        name: read()?,
        version: read()?,
        domain_separator: read()?,
        nonce: read()?,
    };
    Ok(detect_permit(chain_id, token, &reads))
}

/// The EIP-712 payload a Public account signs for `permit` in the token's `domain`.
pub(super) fn permit_typed_data(permit: &Permit, domain: Eip712Domain) -> Result<Value> {
    typed_data::<Permit>(
        domain,
        json!({
            "owner": permit.owner,
            "spender": permit.spender,
            "value": permit.value,
            "nonce": permit.nonce,
            "deadline": permit.deadline,
        }),
    )
}

/// The pre-hook that submits `owner`'s signed `permit` of `token` to `spender`: the token's
/// own `permit` call, declared with `gas_limit`.
pub(super) fn permit_pre_hook(
    token: Address,
    owner: Address,
    spender: Address,
    permit: &PublicSwapPermit,
    gas_limit: u64,
) -> Result<AppDataHook> {
    let signature = Signature::from_raw_array(permit.signature())
        .map_err(|_| eyre!("the swap's signed permit is invalid"))?;
    Ok(AppDataHook {
        call_data: permit_calldata(
            &Permit {
                owner,
                spender,
                value: permit.value(),
                nonce: permit.nonce(),
                deadline: U256::from(permit.deadline()),
            },
            &signature,
        ),
        gas_limit,
        target: token,
    })
}

/// A permit pre-hook of the signed one's length: every argument of `permit` is a static ABI
/// word. It names no account and carries no signature.
pub(super) fn placeholder_permit_pre_hook(gas_limit: u64) -> AppDataHook {
    AppDataHook {
        call_data: IERC20Permit::permitCall {
            owner: Address::ZERO,
            spender: Address::ZERO,
            value: U256::ZERO,
            deadline: U256::ZERO,
            v: 0,
            r: B256::ZERO,
            s: B256::ZERO,
        }
        .abi_encode()
        .into(),
        gas_limit,
        target: Address::ZERO,
    }
}

/// Whether `call`, a signed `permit`, executes at the latest block, from the first endpoint of
/// `pool` that says so. An endpoint's own error says nothing of the permit.
async fn permit_executes(pool: &QueryRpcPool, call: &TransactionRequest) -> Result<bool> {
    for endpoint in pool.available_providers() {
        match execute(&endpoint.provider, call.clone()).await {
            Ok(Execution::Succeeded) => return Ok(true),
            Ok(Execution::Reverted(_)) => return Ok(false),
            Ok(Execution::OutOfGas) | Err(_) => {}
        }
    }
    Err(eyre!("the network the swap pays on is unavailable"))
}

/// What a signed approval grants, for a review beside a hardware device's prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicSwapPermitTerms {
    /// The token the swap sells.
    pub token: Address,
    /// `CoW`'s vault relayer, which takes the sold amount.
    pub spender: Address,
    pub amount: U256,
    /// Unix seconds after which the permit can't be used: the order's `validTo`.
    pub deadline: u32,
}

impl ExecutorOwner {
    fn permit_support(&self) -> Result<MutexGuard<'_, BTreeMap<(u64, Address), PermitSupport>>> {
        self.permit_support
            .lock()
            .map_err(|_| eyre!("the session's permit findings are unavailable"))
    }

    fn permit_warnings(&self) -> Result<MutexGuard<'_, BTreeSet<SwapUseId>>> {
        self.permit_used_up
            .lock()
            .map_err(|_| eyre!("the session's permit findings are unavailable"))
    }

    /// How the Public account `source` lets `spender` take `amount` of `token` on the origin
    /// chain, read now: for the review, and again before sending or signing.
    ///
    /// Only an `order` can carry a permit, so only its token is asked for one, in the same
    /// request as the allowance: everything that decides it for a token this session hasn't
    /// seen, the account's nonce alone for a token with a confirmed domain, and nothing more
    /// for a token without a permit. A read that failed plans transactions this time and is
    /// asked again by the next plan.
    pub(crate) async fn public_swap_approval_plan(
        &self,
        origin: &EffectiveChainConfig,
        source: Address,
        token: Address,
        spender: Address,
        amount: U256,
        order: bool,
    ) -> Result<PublicSwapApprovalPlan> {
        self.ensure_active()?;
        // The native asset is deposited as value and takes no approval.
        if token == Address::ZERO {
            return Ok(PublicSwapApprovalPlan::None);
        }
        if !order {
            let allowance = self
                .public_swap_allowance(origin, source, token, spender)
                .await?;
            return Ok(PublicSwapApprovalPlan::new(allowance, amount, None));
        }
        let key = (origin.chain_id, token);
        let known = self.permit_support()?.get(&key).cloned();
        let mut calls = vec![(
            token,
            PublicErc20::allowanceCall {
                owner: source,
                spender,
            }
            .abi_encode()
            .into(),
        )];
        match &known {
            None => calls.extend(detection_calls(token, source)),
            Some(PermitSupport::Domain(_)) => calls.push((
                token,
                IERC20Permit::noncesCall { owner: source }
                    .abi_encode()
                    .into(),
            )),
            Some(PermitSupport::Absent) => {}
        }
        let mut results = self
            .while_active(async {
                Ok(self
                    .http
                    .rpc_broker()
                    .submit_eth_calls(
                        RpcRoute::from(origin.rpc_route.clone()),
                        calls,
                        WalletRpcOrigin::PublicWallet.into(),
                    )
                    .await?)
            })
            .await
            .wrap_err(ALLOWANCE_READ)?
            .into_iter();
        let allowance = results
            .next()
            .ok_or_else(|| eyre!("the allowance read returned nothing"))?
            .wrap_err(ALLOWANCE_READ)?;
        let allowance = PublicErc20::allowanceCall::abi_decode_returns_validate(&allowance)
            .wrap_err(ALLOWANCE_READ)?;
        let permit = match known {
            Some(PermitSupport::Absent) => None,
            Some(PermitSupport::Domain(domain)) => results
                .next()
                .and_then(Result::ok)
                .and_then(|output| IERC20Permit::noncesCall::abi_decode_returns(&output).ok())
                .map(|nonce| PublicSwapPermitPlan { domain, nonce }),
            None => match detect_permit_from(origin.chain_id, token, &mut results)? {
                PermitDetection::Confirmed { domain, nonce } => {
                    self.permit_support()?
                        .insert(key, PermitSupport::Domain(domain.clone()));
                    Some(PublicSwapPermitPlan { domain, nonce })
                }
                PermitDetection::Absent => {
                    self.permit_support()?.insert(key, PermitSupport::Absent);
                    None
                }
                PermitDetection::Unknown => None,
            },
        };
        Ok(PublicSwapApprovalPlan::new(allowance, amount, permit))
    }

    /// Sign the permit of an order that was approved with one, and prove it: `signer`'s
    /// approval of exactly `value` of `token` to `spender` until `valid_to`, under its current
    /// nonce. `None` when the allowance covers the amount by now, so the order needs no
    /// pre-hook.
    ///
    /// The signed call is then run on the token in a standalone `eth_call` at the latest
    /// block, outside the broker's shared reads and with nothing cached. A permit that
    /// reverts there is an error, and nothing more is signed. The token is then asked for its
    /// permit again, whatever the session holds of it. Another domain than the signed one
    /// replaces it, and the next attempt signs under that. The same domain at the signed
    /// nonce means the token's `permit` isn't the standard one, as DAI's, and the session
    /// plans transactions for it from then on. A transient cause, such as a paused token or
    /// a lagging endpoint, isn't told apart from that and ends the same way. A nonce that
    /// moved says nothing of the permit and changes nothing. A token that can't be read is
    /// forgotten, so the next plan decides it afresh.
    pub(super) async fn sign_public_swap_permit(
        &self,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        token: Address,
        spender: Address,
        value: U256,
        valid_to: u32,
        hash_fallback_confirmed: bool,
    ) -> Result<Option<PublicSwapPermit>> {
        let owner = signer.address();
        let plan = match self
            .public_swap_approval_plan(origin, owner, token, spender, value, true)
            .await?
        {
            PublicSwapApprovalPlan::None => return Ok(None),
            PublicSwapApprovalPlan::Permit(plan) => plan,
            PublicSwapApprovalPlan::Transactions(_) => {
                return Err(eyre!(
                    "the Public account's approval can no longer be signed; review the swap again"
                ));
            }
        };
        let permit = Permit {
            owner,
            spender,
            value,
            nonce: plan.nonce,
            deadline: U256::from(valid_to),
        };
        let signature = self
            .while_active(sign_public_swap_typed_data(
                signer,
                permit_typed_data(&permit, plan.domain.clone())?,
                permit.eip712_signing_hash(&plan.domain),
                hash_fallback_confirmed,
            ))
            .await?;
        let pool = query_rpc_pool_with_http_client(origin.rpc_route.endpoint_urls(), &self.http);
        let call = TransactionRequest::default()
            .to(token)
            .input(permit_calldata(&permit, &signature).into());
        if !self.while_active(permit_executes(&pool, &call)).await? {
            let key = (origin.chain_id, token);
            let detection = self.redetect_permit(origin, token, owner).await;
            let mut support = self.permit_support()?;
            match detection {
                PermitDetection::Confirmed { domain, .. } if domain != plan.domain => {
                    support.insert(key, PermitSupport::Domain(domain));
                }
                // A nonce that moved since it was read says nothing of the token's permit.
                PermitDetection::Confirmed { nonce, .. } if nonce != plan.nonce => {}
                PermitDetection::Confirmed { .. } | PermitDetection::Absent => {
                    support.insert(key, PermitSupport::Absent);
                }
                PermitDetection::Unknown => {
                    support.remove(&key);
                }
            }
            return Err(eyre!(PERMIT_UNUSABLE));
        }
        Ok(Some(PublicSwapPermit::new(
            plan.nonce,
            valid_to,
            value,
            signature.as_bytes(),
        )))
    }

    /// What `token` answers of its permit for `owner` now, whatever the session holds of it:
    /// the reads of [`detection_calls`] in one request. `Unknown` when the request fails.
    async fn redetect_permit(
        &self,
        origin: &EffectiveChainConfig,
        token: Address,
        owner: Address,
    ) -> PermitDetection {
        let results = self
            .while_active(async {
                Ok(self
                    .http
                    .rpc_broker()
                    .submit_eth_calls(
                        RpcRoute::from(origin.rpc_route.clone()),
                        detection_calls(token, owner).to_vec(),
                        WalletRpcOrigin::PublicWallet.into(),
                    )
                    .await?)
            })
            .await;
        results
            .and_then(|results| {
                detect_permit_from(origin.chain_id, token, &mut results.into_iter())
            })
            .unwrap_or(PermitDetection::Unknown)
    }

    /// `owner`'s permit nonce on `token`, its allowance to `spender` and `settlement`'s
    /// `filledAmount` of the order `uid` on the origin chain, in one request, so that the
    /// three are read at one block. `None` when any of them can't be read.
    async fn public_swap_permit_state(
        &self,
        origin: &EffectiveChainConfig,
        token: Address,
        owner: Address,
        spender: Address,
        settlement: Address,
        uid: OrderUid,
    ) -> Option<(U256, U256, U256)> {
        let calls = vec![
            (
                token,
                IERC20Permit::noncesCall { owner }.abi_encode().into(),
            ),
            (
                token,
                PublicErc20::allowanceCall { owner, spender }
                    .abi_encode()
                    .into(),
            ),
            (
                settlement,
                SwapSettlement::filledAmountCall {
                    orderUid: uid.0.to_vec().into(),
                }
                .abi_encode()
                .into(),
            ),
        ];
        let mut results = self
            .while_active(async {
                Ok(self
                    .http
                    .rpc_broker()
                    .submit_eth_calls(
                        RpcRoute::from(origin.rpc_route.clone()),
                        calls,
                        WalletRpcOrigin::PublicWallet.into(),
                    )
                    .await?)
            })
            .await
            .ok()?
            .into_iter();
        let nonce = IERC20Permit::noncesCall::abi_decode_returns(&results.next()?.ok()?).ok()?;
        let allowance =
            PublicErc20::allowanceCall::abi_decode_returns(&results.next()?.ok()?).ok()?;
        let filled =
            SwapSettlement::filledAmountCall::abi_decode_returns(&results.next()?.ok()?).ok()?;
        Some((nonce, allowance, filled))
    }

    /// What the permit of the claimed swap's order grants, for a review before the Public
    /// account's device prompt: the saved approval's sold token and amount to the origin
    /// chain's vault relayer, until `valid_to`. `None` for a swap approved without a permit.
    pub fn public_swap_order_permit_terms(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        valid_to: u32,
    ) -> Result<Option<PublicSwapPermitTerms>> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let approval = claimed.swap.approval();
        if !claimed.swap.intent().order || approval.bounds.pre_hook_gas_limit == 0 {
            return Ok(None);
        }
        let profile = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!(NO_PUBLIC_SWAPS))?;
        Ok(Some(PublicSwapPermitTerms {
            token: approval.sell_token,
            spender: profile.vault_relayer(),
            amount: approval.bounds.sell_amount,
            deadline: valid_to,
        }))
    }

    /// Whether the signed approval of the swap `swap_use`'s open order was used up: the
    /// account's permit nonce moved past the signed one, its allowance doesn't cover the
    /// sale and the settlement has filled nothing of the order, so the order can't currently
    /// fill. Session state from the latest tracking step.
    /// It finishes nothing and releases nothing.
    #[must_use]
    pub fn public_swap_permit_used_up(&self, swap_use: SwapUseId) -> bool {
        self.permit_used_up
            .lock()
            .is_ok_and(|warnings| warnings.contains(&swap_use))
    }

    /// Read whether the permit of `swap`'s order can still work, and return whether
    /// [`Self::public_swap_permit_used_up`] changed.
    ///
    /// Only an order with a signed permit that can still fill is read: the warning of any
    /// other swap is cleared. It is raised when the nonce is past the signed one, the
    /// allowance is short and the settlement has filled nothing of the order, and cleared
    /// otherwise, however that came about. A settlement uses the nonce and the allowance as
    /// well, before the record holds its trade final, so the settlement contract's
    /// `filledAmount` of the order is read with them in one request: a fill raises nothing.
    /// A read that fails leaves the warning as it was.
    pub(super) async fn observe_public_swap_permit(
        &self,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: Address,
        swap: &PublicSwapRecord,
        now: u64,
    ) -> Result<bool> {
        let open = swap
            .order()
            .filter(|_| swap.order_can_fill(now))
            .and_then(|order| Some((order, order.permit()?)));
        let Some((order, permit)) = open else {
            return Ok(self.permit_warnings()?.remove(&swap_use));
        };
        let Some(profile) = origin.public_swap_profile() else {
            return Ok(false);
        };
        let approval = swap.approval();
        let Some((nonce, allowance, filled)) = self
            .public_swap_permit_state(
                origin,
                approval.sell_token,
                source,
                profile.vault_relayer(),
                profile.settlement(),
                order.uid(),
            )
            .await
        else {
            return Ok(false);
        };
        let used_up =
            nonce > permit.nonce() && allowance < approval.bounds.sell_amount && filled.is_zero();
        let mut warnings = self.permit_warnings()?;
        Ok(if used_up {
            warnings.insert(swap_use)
        } else {
            warnings.remove(&swap_use)
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, keccak256, uint};
    use alloy::sol_types::SolValue as _;
    use railgun_wallet::tx::GasEstimateMode;

    use super::super::order::SwapPrice;
    use super::super::public_order::{price_public_order, public_order_hook_gas};
    use super::super::public_transactions::public_swap_gas_plan;
    use super::*;
    use crate::cow::{
        CowQuote, GAS_SHARE_BALANCED_BPS, PUBLIC_PERMIT_HOOK_GAS, public_deposit_hook_gas,
    };
    use crate::{PairAnchorRate, RpcRevert};

    const CHAIN: u64 = 8453;
    const TOKEN: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const NONCE: U256 = uint!(7_U256);

    fn reverted() -> Read {
        Err(RpcBrokerError::InnerRevert(RpcRevert::from_multicall(
            Bytes::new(),
        )))
    }

    // Returns a read's result, as the other helpers do.
    #[allow(clippy::unnecessary_wraps)]
    fn text(value: &str) -> Read {
        Ok(IERC20Permit::nameCall::abi_encode_returns(&value.to_owned()).into())
    }

    /// The reads of a token without `eip712Domain()` that reports `separator`.
    fn token(name: Read, version: Read, separator: B256) -> PermitReads {
        PermitReads {
            eip712_domain: reverted(),
            name,
            version,
            domain_separator: Ok(IERC20Permit::DOMAIN_SEPARATORCall::abi_encode_returns(
                &separator,
            )
            .into()),
            nonce: Ok(IERC20Permit::noncesCall::abi_encode_returns(&NONCE).into()),
        }
    }

    fn domain(
        name: &'static str,
        version: Option<&'static str>,
        chain_id: Option<u64>,
        salt: Option<B256>,
    ) -> Eip712Domain {
        Eip712Domain::new(
            Some(Cow::Borrowed(name)),
            version.map(Cow::Borrowed),
            chain_id.map(U256::from),
            Some(TOKEN),
            salt,
        )
    }

    // A token's permit is confirmed only under a domain whose hash is the separator the token
    // reports, each one encoded here by hand as its contract computes it. Anything else the
    // token answers means it has no permit, and a read that failed decides nothing.
    #[test]
    fn a_permit_is_confirmed_under_the_domain_that_reproduces_the_tokens_separator() {
        let (name, two, one) = (keccak256("Token"), keccak256("2"), keccak256("1"));
        let chain = U256::from(CHAIN);
        let confirmed = |domain| PermitDetection::Confirmed {
            domain,
            nonce: NONCE,
        };

        // EIP-5267 names its fields: here a domain without a version.
        let separator = keccak256(
            (
                keccak256("EIP712Domain(string name,uint256 chainId,address verifyingContract)"),
                name,
                chain,
                TOKEN,
            )
                .abi_encode(),
        );
        let reported = IERC20Permit::eip712DomainReturn {
            fields: [0x0d].into(),
            name: "Token".to_owned(),
            version: "unused".to_owned(),
            chainId: chain,
            verifyingContract: TOKEN,
            salt: B256::ZERO,
            extensions: Vec::new(),
        };
        let reads = PermitReads {
            eip712_domain: Ok(IERC20Permit::eip712DomainCall::abi_encode_returns(&reported).into()),
            ..token(reverted(), reverted(), separator)
        };
        assert_eq!(
            detect_permit(CHAIN, TOKEN, &reads),
            confirmed(domain("Token", None, Some(CHAIN), None))
        );
        // A reported domain that names another contract or another chain is some other
        // token's. Its separator is that domain's, and it is still no permit of this token.
        for (contract, chain_id) in [
            (Address::repeat_byte(0x11), chain),
            (TOKEN, U256::from(CHAIN + 1)),
        ] {
            let separator = keccak256(
                (
                    keccak256(
                        "EIP712Domain(string name,uint256 chainId,address verifyingContract)",
                    ),
                    name,
                    chain_id,
                    contract,
                )
                    .abi_encode(),
            );
            let reported = IERC20Permit::eip712DomainReturn {
                chainId: chain_id,
                verifyingContract: contract,
                ..reported.clone()
            };
            let reads = PermitReads {
                eip712_domain: Ok(
                    IERC20Permit::eip712DomainCall::abi_encode_returns(&reported).into(),
                ),
                ..token(reverted(), reverted(), separator)
            };
            assert_eq!(detect_permit(CHAIN, TOKEN, &reads), PermitDetection::Absent);
        }

        // `name()` and `version()`, and version "1" for a token without `version()`.
        let versioned =
            "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
        for (version, hash, expected) in [(text("2"), two, "2"), (reverted(), one, "1")] {
            let separator =
                keccak256((keccak256(versioned), name, hash, chain, TOKEN).abi_encode());
            assert_eq!(
                detect_permit(CHAIN, TOKEN, &token(text("Token"), version, separator)),
                confirmed(domain("Token", Some(expected), Some(CHAIN), None))
            );
        }

        // A bridged token's domain carries the chain id as its salt.
        let salt = B256::from(chain);
        let separator = keccak256(
            (
                keccak256(
                    "EIP712Domain(string name,string version,address verifyingContract,bytes32 salt)",
                ),
                name,
                one,
                TOKEN,
                salt,
            )
                .abi_encode(),
        );
        assert_eq!(
            detect_permit(CHAIN, TOKEN, &token(text("Token"), text("1"), separator)),
            confirmed(domain("Token", Some("1"), None, Some(salt)))
        );

        // A separator no candidate reproduces, as DAI's, is no permit the wallet can sign.
        let other = B256::repeat_byte(9);
        assert_eq!(
            detect_permit(CHAIN, TOKEN, &token(text("Token"), text("2"), other)),
            PermitDetection::Absent
        );
        // A read that failed decides nothing, whatever the others answered.
        let failed = || Err(RpcBrokerError::Timeout);
        let mut unread = token(text("Token"), text("2"), other);
        unread.nonce = failed();
        assert_eq!(
            detect_permit(CHAIN, TOKEN, &unread),
            PermitDetection::Unknown
        );
        assert_eq!(
            detect_permit(CHAIN, TOKEN, &token(failed(), text("2"), other)),
            PermitDetection::Unknown
        );
    }

    // A covered allowance takes nothing, a short one a permit where the token has one, and
    // otherwise the exact approval, after a reset when the allowance isn't zero. Only the
    // transactions cost the account gas.
    #[test]
    fn the_approval_plan_is_none_a_permit_or_exact_transactions() {
        let amount = U256::from(200);
        let permit = PublicSwapPermitPlan {
            domain: domain("Token", Some("2"), Some(CHAIN), None),
            nonce: NONCE,
        };
        let plan = |allowance: u64, permit: Option<&PublicSwapPermitPlan>| {
            PublicSwapApprovalPlan::new(U256::from(allowance), amount, permit.cloned())
        };
        assert_eq!(plan(200, Some(&permit)), PublicSwapApprovalPlan::None);
        assert_eq!(
            PublicSwapApprovalPlan::new(U256::MAX, amount, None),
            PublicSwapApprovalPlan::None
        );
        for allowance in [0, 100] {
            assert_eq!(
                plan(allowance, Some(&permit)),
                PublicSwapApprovalPlan::Permit(permit.clone())
            );
        }
        assert_eq!(plan(0, None).transactions(), [amount]);
        assert_eq!(plan(100, None).transactions(), [U256::ZERO, amount]);

        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let gas = |plan: &PublicSwapApprovalPlan| {
            public_swap_gas_plan(&chain, plan.transactions().len(), false, 7, 1).unwrap()
        };
        for free in [plan(200, None), plan(0, Some(&permit))] {
            let gas = gas(&free);
            assert!(gas.approval_gas_limits.is_empty());
            assert_eq!(gas.max_gas_cost, U256::ZERO);
        }
        assert_eq!(gas(&plan(100, None)).approval_gas_limits.len(), 2);
    }

    // The order's limit prices the permit pre-hook with its post-hook: at 4 wei per gas, which
    // the limit raises to 5, and one bought unit per wei, the estimate grows by five times the
    // hook's gas.
    #[test]
    fn an_orders_gas_estimate_includes_its_permit_hook() {
        let permit = PublicSwapApprovalPlan::Permit(PublicSwapPermitPlan {
            domain: domain("Token", Some("2"), Some(CHAIN), None),
            nonce: NONCE,
        });
        let estimate = |plan: &PublicSwapApprovalPlan| {
            let quote: CowQuote = serde_json::from_value(json!({
                "quote": {
                    "sellToken": TOKEN, "buyToken": Address::repeat_byte(6),
                    "sellAmount": "997500", "buyAmount": "10000000",
                    "validTo": 1, "feeAmount": "2500", "gasAmount": "100000", "gasPrice": "99",
                    "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
                },
                "expiration": "", "id": 7, "verified": true, "protocolFeeBps": "2"
            }))
            .unwrap();
            let price = SwapPrice::Verified {
                rate: PairAnchorRate {
                    sell_rate: U256::ONE,
                    buy_rate: uint!(1_000_000_000_000_000_000_U256),
                },
                observations: Vec::new(),
            };
            price_public_order(
                quote,
                &price,
                4,
                U256::ZERO,
                public_order_hook_gas(true, plan),
                Address::repeat_byte(6),
                100,
                GAS_SHARE_BALANCED_BPS,
                Address::repeat_byte(0x70),
                0,
                1_800,
            )
            .unwrap()
            .limit
            .gas_estimate
        };
        assert_eq!(
            public_order_hook_gas(true, &PublicSwapApprovalPlan::None),
            public_deposit_hook_gas(true, GasEstimateMode::UpperBound)
        );
        assert_eq!(
            estimate(&permit) - estimate(&PublicSwapApprovalPlan::None),
            U256::from(PUBLIC_PERMIT_HOOK_GAS) * U256::from(5)
        );
    }
}
