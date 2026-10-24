use super::*;
use crate::rpc_broker::capacity::{CapacityLedger, JobSlots, SlotKind};
use crate::rpc_broker::model::{
    RpcChainRoute, RpcOrigin, RpcRead, RpcRoute, RpcSubmission, WalletRpcOrigin,
};
use crate::rpc_broker::resolution::{WaiterPolicy, WaiterState};
use crate::rpc_broker::tests::read_calldata;
use alloy::primitives::{Address, Bytes};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
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
        RpcRoute::new(chain.with_multicall(Address::from([9; 20])))
    } else {
        RpcRoute::new(chain)
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
fn repeated_expiry_prunes_origin_metadata_of_work_that_cannot_read() {
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
        assert!(scheduler.next(|_| false).is_none());
        assert_eq!(scheduler.expire(Instant::now()).len(), 1);
        assert!(scheduler.jobs.is_empty());
        assert!(scheduler.lanes.is_empty());
        assert!(scheduler.origins.is_empty());
    }
}

#[test]
fn blocked_lane_head_keeps_its_place_without_holding_back_ready_work() {
    let route = route(false);
    let origin_a = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let origin_b = RpcOrigin::from(WalletRpcOrigin::Staking);
    let mut scheduler = ReadyScheduler::new();
    scheduler.admit_chunks(vec![
        vec![item(route.clone(), b"a-blocked", origin_a.clone(), None, 1)],
        vec![item(route.clone(), b"a-ready", origin_a.clone(), None, 2)],
        vec![item(route.clone(), b"b-ready", origin_b, None, 3)],
        vec![item(route, b"a-later", origin_a, None, 4)],
    ]);
    let verified = std::cell::Cell::new(false);
    let marker = |job: &ExecutionJob| read_calldata(&job.first().expect("job item").read);
    let ready =
        |job: &ExecutionJob| verified.get() || marker(job) != Bytes::from_static(b"a-blocked");
    let claim = |scheduler: &mut ReadyScheduler| match scheduler.next(ready) {
        Some(Claim::Queued(job)) => Some(marker(&job)),
        Some(Claim::Resume(_)) => panic!("only queued work was admitted"),
        None => None,
    };

    // The blocked head is skipped, and origin B still gets the next turn.
    assert_eq!(claim(&mut scheduler), Some(Bytes::from_static(b"a-ready")));
    assert_eq!(claim(&mut scheduler), Some(Bytes::from_static(b"b-ready")));
    // Once it can read, the retained entry is claimed ahead of later work in its lane.
    verified.set(true);
    assert_eq!(
        claim(&mut scheduler),
        Some(Bytes::from_static(b"a-blocked"))
    );
    assert_eq!(claim(&mut scheduler), Some(Bytes::from_static(b"a-later")));
    assert_eq!(claim(&mut scheduler), None);
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
    let job = scheduler.next_ready().expect("surviving aggregate");
    let ExecutionJob::Aggregate(items) = job else {
        panic!("expected surviving aggregate");
    };
    assert_eq!(items.len(), 1);
    assert!(scheduler.jobs.is_empty());
    assert!(scheduler.lanes.is_empty());
    assert!(scheduler.origins.is_empty());
}

/// A started individual job's read request for `keys`. Dropping the receiver closes it.
fn resume_request(keys: Vec<WorkKey>) -> (SlotRequest, oneshot::Receiver<()>) {
    let slots = JobSlots::new(CapacityLedger::new());
    let (grant, granted) = oneshot::channel();
    let request = SlotRequest {
        kind: SlotKind::Read,
        slots: Arc::downgrade(&slots),
        grant,
        aggregate: false,
        keys,
    };
    (request, granted)
}

#[test]
fn late_caller_joins_resuming_job_without_losing_a_newer_admission() {
    let route = route(false);
    let origin_a = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let origin_b = RpcOrigin::from(WalletRpcOrigin::Staking);
    let parked = item(route.clone(), b"parked", origin_a.clone(), None, 3);
    let key = parked.key.clone();
    let mut scheduler = ReadyScheduler::new();
    scheduler.admit_chunks(vec![
        vec![item(route.clone(), b"a-first", origin_a.clone(), None, 1)],
        vec![item(route, b"a-second", origin_a.clone(), None, 2)],
    ]);
    let (request, _granted) = resume_request(vec![key.clone()]);
    scheduler.admit_resume(request, vec![origin_a.clone()]);
    let marker = |job: &ExecutionJob| read_calldata(&job.first().expect("job item").read);
    let claim = |scheduler: &mut ReadyScheduler| match scheduler.next(|_| true) {
        Some(Claim::Queued(job)) => Some(marker(&job)),
        Some(Claim::Resume(_)) => Some(Bytes::from_static(b"resume")),
        None => None,
    };

    // B joined after the job parked, so B's turn serves the resume ahead of A's backlog.
    assert!(scheduler.attach(&key, &origin_b));
    assert_eq!(claim(&mut scheduler), Some(Bytes::from_static(b"a-first")));
    assert_eq!(claim(&mut scheduler), Some(Bytes::from_static(b"resume")));
    assert_eq!(claim(&mut scheduler), Some(Bytes::from_static(b"a-second")));
    assert_eq!(claim(&mut scheduler), None);

    // An abandoned request is still queued when its work is readmitted under the same key.
    let (request, granted) = resume_request(vec![key.clone()]);
    scheduler.admit_resume(request, vec![origin_a]);
    drop(granted);
    scheduler.admit_chunks(vec![vec![parked]]);
    assert!(scheduler.expire(Instant::now()).is_empty());
    // Pruning the abandoned entry leaves the replacement indexed and claimable once.
    assert!(scheduler.attach(&key, &origin_b));
    assert_eq!(claim(&mut scheduler), Some(Bytes::from_static(b"parked")));
    assert_eq!(claim(&mut scheduler), None);
}
