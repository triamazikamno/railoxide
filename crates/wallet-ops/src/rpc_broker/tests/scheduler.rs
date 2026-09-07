use super::*;
use crate::rpc_broker::model::{
    RpcChainRoute, RpcOrigin, RpcRead, RpcRoute, RpcSubmission, WalletRpcOrigin,
};
use crate::rpc_broker::resolution::{WaiterPolicy, WaiterState};
use alloy::primitives::{Address, Bytes};
use std::time::Duration;
use tokio::time::Instant;
use url::Url;

fn item(
    route: RpcRoute,
    marker: &'static [u8],
    origin: RpcOrigin,
    deadline: Option<Instant>,
    nonce: u64,
) -> WorkItem {
    let read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(marker));
    let key = WorkKey {
        identity: read.identity_for_route(&route),
        route: route.chain_route().clone(),
        nonce,
        latest_epoch: None,
    };
    WorkItem {
        key,
        execution_route: route,
        read,
        origins: vec![origin],
        waiters: WaiterState::new(WaiterPolicy {
            deadline,
            attempt_timeout: Duration::from_secs(1),
        }),
    }
}

fn route(multicall: bool) -> RpcRoute {
    let chain = RpcChainRoute::new(
        1,
        vec![Url::parse("https://scheduler-test.invalid").unwrap()],
    );
    if multicall {
        RpcRoute::from(chain.with_multicall(Address::from([9; 20])))
    } else {
        RpcRoute::from(chain)
    }
}

#[test]
fn mixed_reads_share_gas_estimates_for_admission_and_partitioning() {
    let route = route(true).with_test_thresholds(64, 140_000);
    let origin = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let mut reads = vec![
        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"call")),
        RpcRead::get_balance(Address::from([1; 20])),
    ];
    let submission = RpcSubmission::new(route.clone(), reads.clone(), origin.clone());
    assert!(!RouteLoad::from(&submission).exceeds(&route));

    reads.push(RpcRead::get_balance(Address::from([2; 20])));
    let submission = RpcSubmission::new(route.clone(), reads.clone(), origin.clone());
    assert!(RouteLoad::from(&submission).exceeds(&route));

    let work = reads
        .into_iter()
        .map(|read| WorkItem {
            key: WorkKey {
                identity: read.identity_for_route(&route),
                route: route.chain_route().clone(),
                nonce: 0,
                latest_epoch: None,
            },
            execution_route: route.clone(),
            read,
            origins: vec![origin.clone()],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: None,
                attempt_timeout: Duration::from_secs(1),
            }),
        })
        .collect();
    let chunks = partition_work(work);
    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);
}

#[test]
fn repeated_expiry_prunes_origin_metadata_while_execution_slots_are_full() {
    let route = route(false);
    let origins = [
        WalletRpcOrigin::Anchors,
        WalletRpcOrigin::Governance,
        WalletRpcOrigin::GovernorRewards,
        WalletRpcOrigin::PublicWallet,
        WalletRpcOrigin::Staking,
    ];
    let markers: [&'static [u8]; 5] = [b"a", b"b", b"c", b"d", b"e"];
    let mut scheduler = ReadyScheduler::new();
    for (nonce, (origin, marker)) in origins.into_iter().zip(markers).enumerate() {
        scheduler.admit_chunks(vec![vec![item(
            route.clone(),
            marker,
            RpcOrigin::from(origin),
            Some(Instant::now() - Duration::from_secs(1)),
            nonce as u64,
        )]]);
        assert!(scheduler.next(1, 1).is_none());
        assert_eq!(scheduler.expire(Instant::now()).len(), 1);
        assert!(scheduler.jobs.is_empty());
        assert!(scheduler.lanes.is_empty());
        assert!(scheduler.origins.is_empty());
    }
}

#[test]
fn partial_expiry_and_claim_remove_lost_origin_references() {
    let route = route(true);
    let origin_a = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let origin_b = RpcOrigin::from(WalletRpcOrigin::Staking);
    let origin_c = RpcOrigin::from(WalletRpcOrigin::Governance);
    let mut scheduler = ReadyScheduler::new();
    scheduler.admit_chunks(vec![vec![
        item(
            route.clone(),
            b"expired",
            origin_a.clone(),
            Some(Instant::now() - Duration::from_secs(1)),
            1,
        ),
        item(route.clone(), b"survivor", origin_b.clone(), None, 2),
    ]]);
    let survivor_key = WorkKey {
        identity: RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"survivor"))
            .identity_for_route(&route),
        route: route.chain_route().clone(),
        nonce: 2,
        latest_epoch: None,
    };
    assert!(scheduler.attach(&survivor_key, &origin_c));

    assert_eq!(scheduler.expire(Instant::now()).len(), 1);
    assert!(!scheduler.lanes.contains_key(&origin_a));
    assert!(scheduler.lanes.contains_key(&origin_b));
    assert!(scheduler.lanes.contains_key(&origin_c));
    let job = scheduler.next(0, 1).expect("surviving aggregate");
    let ExecutionJob::Aggregate(items) = job else {
        panic!("expected surviving aggregate");
    };
    assert_eq!(items.len(), 1);
    assert!(scheduler.jobs.is_empty());
    assert!(scheduler.lanes.is_empty());
    assert!(scheduler.origins.is_empty());
}
