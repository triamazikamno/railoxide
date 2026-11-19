#[allow(clippy::wildcard_imports)]
use super::*;

impl DappProvider {
    pub(in crate::gateway) const fn authority(&self) -> &watch::Receiver<GatewayWalletState> {
        &self.authority
    }

    /// Final synchronous transport check. Never borrow the watch or approval control here:
    /// the caller holds its watch read guard through the socket operation.
    pub(in crate::gateway) fn delivery_authority(
        &self,
        session: u64,
        delivery: &mut Delivery,
        authority: &GatewayWalletState,
    ) -> DeliveryStatus {
        if let Some(request) = &delivery.chain_editor {
            return if request.session == session && request.is_current_authority(authority) {
                DeliveryStatus::Current
            } else {
                DeliveryStatus::Discard
            };
        }
        match &mut delivery.message {
            GatewayServerMessage::ProviderResponse {
                document,
                generation,
                document_generation,
                outcome,
                ..
            } => {
                if delivery.registration_rejection && delivery.incarnation.is_none() {
                    return DeliveryStatus::Current;
                }
                let Some(doc) = self.documents.get(&(session, document.clone())) else {
                    return DeliveryStatus::Discard;
                };
                if delivery.incarnation != Some(doc.incarnation) {
                    return DeliveryStatus::Discard;
                }
                if delivery.local_error || authority.same_authority(&self.wallet) {
                    return DeliveryStatus::Current;
                }
                *outcome = GatewayProviderOutcome::Failure {
                    error: safe_error(if authority.view.is_none() && self.wallet.view.is_some() {
                        4100
                    } else {
                        -32002
                    }),
                };
                *generation = self.generation;
                *document_generation = doc.generation;
                delivery.local_error = true;
                DeliveryStatus::Changed
            }
            GatewayServerMessage::ProviderState { .. }
            | GatewayServerMessage::UiSnapshot { .. }
            | GatewayServerMessage::State { .. }
                if !authority.same_authority(&self.wallet) =>
            {
                DeliveryStatus::Discard
            }
            _ => DeliveryStatus::Current,
        }
    }

    /// Revalidate before sealing and before each bounded socket progress step.
    pub(in crate::gateway) fn delivery(
        &self,
        session: u64,
        delivery: &mut Delivery,
    ) -> DeliveryStatus {
        match &mut delivery.message {
            GatewayServerMessage::ProviderResponse {
                document,
                generation,
                document_generation,
                outcome,
                ..
            } => {
                if delivery.registration_rejection && delivery.incarnation.is_none() {
                    return DeliveryStatus::Current;
                }
                let Some(doc) = self.documents.get(&(session, document.clone())) else {
                    return DeliveryStatus::Discard;
                };
                if delivery.incarnation != Some(doc.incarnation) {
                    return DeliveryStatus::Discard;
                }
                if delivery.local_error {
                    return DeliveryStatus::Current;
                }
                let invalid = if let Some(approval) = &delivery.approval {
                    approval
                        .control
                        .ensure_current()
                        .err()
                        .map(|error| approvals::broker_code(&error))
                        .or_else(|| {
                            self.validate_owner(
                                &approval.owner,
                                true,
                                approval.deadline,
                                Instant::now(),
                            )
                            .err()
                        })
                } else if let Some((ticket, owner, remote)) = &delivery.read {
                    self.validate_owner(owner, *remote, ticket.deadline, Instant::now())
                        .err()
                } else if *generation != self.generation || *document_generation != doc.generation {
                    Some(if self.wallet.view.is_none() {
                        4100
                    } else {
                        -32002
                    })
                } else {
                    None
                };
                if let Some(code) = invalid {
                    *outcome = GatewayProviderOutcome::Failure {
                        error: safe_error(code),
                    };
                    *generation = self.generation;
                    *document_generation = doc.generation;
                    delivery.local_error = true;
                    return DeliveryStatus::Changed;
                }
            }
            GatewayServerMessage::ProviderState {
                document,
                generation,
                document_generation,
                accounts,
                chain_id,
                ..
            } => {
                let Some(doc) = self.documents.get(&(session, document.clone())) else {
                    return DeliveryStatus::Discard;
                };
                if delivery.incarnation != Some(doc.incarnation)
                    || *generation != self.generation
                    || *document_generation != doc.generation
                    || *accounts != doc.disclosure.accounts
                    || *chain_id != doc.disclosure.chain_id
                {
                    return DeliveryStatus::Discard;
                }
            }
            GatewayServerMessage::UiSnapshot { .. } => {
                // Includes account labels and pending approval ownership, not just the wallet epoch.
                if serde_json::to_value(&delivery.message).ok()
                    != serde_json::to_value(self.ui(session)).ok()
                {
                    return DeliveryStatus::Discard;
                }
            }
            GatewayServerMessage::State { generation, .. } if *generation != self.generation => {
                return DeliveryStatus::Discard;
            }
            _ => {}
        }
        self.delivery_authority(session, delivery, &self.authority.borrow())
    }
    pub(in crate::gateway) fn delivered(&mut self, id: u64) {
        if let Some(read) = self.reads.get_mut(&id) {
            if read.phase == ReadPhase::Running {
                read.delivered = true;
            } else if read.phase == ReadPhase::Delivery {
                self.reads.remove(&id);
                self.finish_ticket(id);
            }
        }
    }
}
