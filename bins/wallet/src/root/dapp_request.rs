use std::future::Future;
use std::pin::Pin;

use serde_json::Value;
use wallet_ops::dapp_request::DappRequestControl;
use wallet_ops::gateway::{GatewayApprovalFailure, GatewayHandle, LocalProviderFailure};
use wallet_ops::vault::PublicAccountScope;
use wallet_ops::vault::PublicAccountSource;
use wallet_ops::{
    DappRpcReadClient, WalletConnectParsedRequest, WalletConnectPendingRequest,
    WalletConnectRequestErrorKind,
};

#[derive(Clone)]
pub(super) struct DappRequestBinding {
    pub(super) public_account_uuid: String,
    pub(super) public_account_scope: PublicAccountScope,
    pub(super) owning_private_wallet_uuid: Option<String>,
    pub(super) peer_name: String,
    pub(super) peer_url: String,
}

#[derive(Clone)]
pub(super) struct DappSessionIdentity {
    pub(super) transport: &'static str,
    pub(super) id: String,
}

#[derive(Clone)]
pub(super) struct DappRequestUi {
    pub(super) key: String,
    pub(super) review_token: u64,
    pub(super) binding: DappRequestBinding,
    pub(super) session_identity: DappSessionIdentity,
    pub(super) parsed: WalletConnectParsedRequest,
    pub(super) item: WalletConnectPendingRequest,
    pub(super) account_source: PublicAccountSource,
    pub(super) request_control: Option<DappRequestControl>,
    pub(super) rpc_reads: Option<DappRpcReadClient>,
}

impl DappRequestUi {
    pub(super) fn approval_admitted(&self, now: u64) -> bool {
        self.request_control.as_ref().map_or_else(
            || self.item.expiry_timestamp.is_none_or(|expiry| expiry > now),
            |control| control.ensure_current().is_ok(),
        )
    }

    // Native approvals use their monotonic control, not the projected display timestamp.
    pub(super) const fn timeout_timestamp(&self) -> Option<u64> {
        if self.request_control.is_some() {
            None
        } else {
            self.item.expiry_timestamp
        }
    }

    pub(super) fn is_current(&self) -> bool {
        self.request_control
            .as_ref()
            .is_none_or(|control| control.ensure_current().is_ok())
    }
}

#[derive(Clone)]
pub(super) enum DappRequestRoute {
    WalletConnect {
        relay_client_id: String,
        topic: String,
        sym_key: [u8; 32],
    },
    Gateway {
        handle: GatewayHandle,
        approval_id: String,
    },
}

pub(super) struct DappRequestError {
    pub(super) kind: WalletConnectRequestErrorKind,
    pub(super) provider_failure: Option<GatewayApprovalFailure>,
    pub(super) message: String,
}

#[derive(Debug)]
pub(super) enum DappApprovalTaskError {
    AdmissionBusy,
    Failed(String),
}

impl From<String> for DappApprovalTaskError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl std::fmt::Display for DappApprovalTaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AdmissionBusy => formatter.write_str("The browser gateway is busy. Review and approve this pending request again to retry."),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

type ResponseFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

pub(super) struct DappResponseSender {
    gateway: Option<(GatewayHandle, String)>,
    send: Box<dyn Fn(Result<Value, DappRequestError>) -> ResponseFuture + Send + Sync>,
}

impl DappResponseSender {
    pub(super) fn new(
        send: impl Fn(Result<Value, DappRequestError>) -> ResponseFuture + Send + Sync + 'static,
    ) -> Self {
        Self {
            gateway: None,
            send: Box::new(send),
        }
    }

    pub(super) fn gateway(handle: GatewayHandle, approval_id: String) -> Self {
        let gateway = Some((handle.clone(), approval_id.clone()));
        let mut sender = Self::new(move |result| {
            let handle = handle.clone();
            let approval_id = approval_id.clone();
            Box::pin(async move {
                handle
                    .complete_approval(
                        approval_id,
                        result.map_err(|error| {
                            error.provider_failure.unwrap_or_else(|| {
                                GatewayApprovalFailure::Local(match error.kind {
                                    WalletConnectRequestErrorKind::UserRejected => {
                                        LocalProviderFailure::UserRejected
                                    }
                                    WalletConnectRequestErrorKind::UnsupportedMethod => {
                                        LocalProviderFailure::Unsupported
                                    }
                                    WalletConnectRequestErrorKind::UnsupportedChain => {
                                        LocalProviderFailure::ChainUnavailable
                                    }
                                    WalletConnectRequestErrorKind::MalformedParams => {
                                        LocalProviderFailure::InvalidParams
                                    }
                                    WalletConnectRequestErrorKind::ExpiredRequest => {
                                        LocalProviderFailure::Unavailable
                                    }
                                    WalletConnectRequestErrorKind::Unauthorized => {
                                        LocalProviderFailure::Unauthorized
                                    }
                                    WalletConnectRequestErrorKind::Internal => {
                                        LocalProviderFailure::Internal
                                    }
                                })
                            })
                        }),
                    )
                    .await
                    .map_err(|_| "Dapp response is no longer available".to_owned())
            })
        });
        sender.gateway = gateway;
        sender
    }

    pub(super) async fn begin_approval(&self) -> Result<(), LocalProviderFailure> {
        if let Some((handle, id)) = &self.gateway {
            handle.begin_approval(id.clone()).await?;
        }
        Ok(())
    }

    pub(super) async fn return_to_review(&self) -> Result<(), String> {
        if let Some((handle, id)) = &self.gateway {
            handle
                .return_approval_to_review(id.clone())
                .await
                .map_err(|_| "Dapp approval is no longer current".to_owned())?;
        }
        Ok(())
    }

    pub(super) async fn send(&self, result: Result<Value, DappRequestError>) -> Result<(), String> {
        (self.send)(result).await
    }
}
