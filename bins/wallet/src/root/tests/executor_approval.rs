use super::*;
use crate::root::private_action::ExecutorUnshieldQuote;
use wallet_ops::{
    DesktopSelfBroadcastCostEstimate, SelfBroadcastGasFeeQuote, SelfBroadcastGasFeeSelection,
};

#[test]
fn executor_broadcaster_approval_only_continues_within_the_displayed_terms() {
    let token = Address::repeat_byte(1);
    let candidate = wallet_ops::public_broadcaster_candidates(
        &[fee_row(1, token, "quote")],
        1,
        token,
        None,
        SystemTime::now(),
        wallet_ops::BroadcasterFeePolicy::default(),
        None,
    )
    .pop()
    .unwrap();
    let mut quote = public_broadcaster_cost_estimate(candidate);
    quote.fee_amount = U256::from(100);
    quote.recipient_amount = U256::from(900);
    quote.total_private_spend = U256::from(1_000);
    let approved = ExecutorUnshieldQuote::Broadcaster(Box::new(quote.clone()));
    let covered = |quote| approved.covers(&ExecutorUnshieldQuote::Broadcaster(Box::new(quote)));
    assert!(covered(quote.clone()));

    let mut cheaper = quote.clone();
    cheaper.fee_amount -= U256::from(1);
    cheaper.recipient_amount += U256::from(1);
    assert!(covered(cheaper));

    let mut higher_fee = quote.clone();
    higher_fee.fee_amount += U256::from(1);
    assert!(!covered(higher_fee));
    let mut lower_received = quote.clone();
    lower_received.recipient_amount -= U256::from(1);
    assert!(!covered(lower_received));
    let mut higher_spend = quote.clone();
    higher_spend.total_private_spend += U256::from(1);
    assert!(!covered(higher_spend));
    let mut another_broadcaster = quote.clone();
    another_broadcaster.broadcaster.railgun_address = "another broadcaster".into();
    assert!(!covered(another_broadcaster));
    let mut another_fee_token = quote;
    another_fee_token.fee_token = Address::repeat_byte(3);
    assert!(!covered(another_fee_token));
}

#[test]
fn executor_self_funded_approval_bounds_gas_and_protocol_fees() {
    let quote = SelfBroadcastGasFeeQuote::from_rpc_gas_price(100);
    let gas_fee = SelfBroadcastGasFeeSelection::Custom {
        max_fee_per_gas: 120,
        max_priority_fee_per_gas: 2,
    };
    let cost = DesktopSelfBroadcastCostEstimate {
        gas_limit: 500_000,
        gas_cost: wallet_ops::eip1559_gas_cost_projection(500_000, quote, 120, 2),
        protocol_fees: vec![wallet_ops::DesktopSelfBroadcastProtocolFee {
            token: Address::repeat_byte(1),
            amount: U256::from(25),
        }],
    };
    let approved = ExecutorUnshieldQuote::SelfBroadcast {
        cost: cost.clone(),
        gas_fee,
    };
    let covered =
        |cost, gas_fee| approved.covers(&ExecutorUnshieldQuote::SelfBroadcast { cost, gas_fee });
    assert!(covered(cost.clone(), gas_fee));
    let mut cheaper = cost.clone();
    cheaper.gas_limit -= 1;
    cheaper.gas_cost.maximum_cost -= U256::from(120);
    assert!(covered(cheaper, gas_fee));

    let mut higher_cost = cost.clone();
    higher_cost.gas_cost.maximum_cost += U256::from(1);
    assert!(!covered(higher_cost, gas_fee));
    let mut higher_limit = cost.clone();
    higher_limit.gas_limit += 1;
    assert!(!covered(higher_limit, gas_fee));
    let mut higher_protocol_fee = cost.clone();
    higher_protocol_fee.protocol_fees[0].amount += U256::from(1);
    assert!(!covered(higher_protocol_fee, gas_fee));
    assert!(!covered(
        cost.clone(),
        SelfBroadcastGasFeeSelection::Custom {
            max_fee_per_gas: 121,
            max_priority_fee_per_gas: 2,
        }
    ));
    assert!(!covered(
        cost,
        SelfBroadcastGasFeeSelection::Custom {
            max_fee_per_gas: 120,
            max_priority_fee_per_gas: 3,
        }
    ));
}
