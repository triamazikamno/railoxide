#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn set_favorites_only_broadcasters(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        enabled: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.set_broadcaster_filter(kind, key, BroadcasterFilter::FavoritesOnly(enabled), cx);
    }

    pub(in crate::root) fn set_allow_suspicious_broadcasters(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        allow: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.set_broadcaster_filter(kind, key, BroadcasterFilter::AllowSuspicious(allow), cx);
    }

    fn set_broadcaster_filter(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        filter: BroadcasterFilter,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => self.set_send_broadcaster_filter(key, filter, cx),
            DeliveryFormKind::Unshield => self.set_unshield_broadcaster_filter(key, filter, cx),
        }
    }

    pub(in crate::root) fn set_transaction_fee_breakdown_open(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.set_fee_breakdown_open(kind, key, FeeBreakdownOpen::TransactionFee(open), cx);
    }

    pub(in crate::root) fn set_sponsored_funding_breakdown_open(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.set_fee_breakdown_open(kind, key, FeeBreakdownOpen::SponsoredFunding(open), cx);
    }

    fn set_fee_breakdown_open(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        change: FeeBreakdownOpen,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                change.apply((
                    &mut form.transaction_fee_breakdown_open,
                    &mut form.sponsored_funding_breakdown_open,
                ))
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                change.apply((
                    &mut form.transaction_fee_breakdown_open,
                    &mut form.sponsored_funding_breakdown_open,
                ))
            }),
        };
        if changed {
            cx.notify();
        }
    }
}
