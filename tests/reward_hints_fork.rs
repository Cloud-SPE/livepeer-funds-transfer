//! End-to-end check of reward hints against the real contracts on an Arbitrum fork.
//!
//! Needs `anvil` (Foundry) on PATH and network access. Run with:
//!   cargo test --test reward_hints_fork -- --ignored --nocapture
//! Env:
//!   ARB_RPC_URL           fork source (default https://arb1.arbitrum.io/rpc)
//!   FORK_TEST_SKIP        pool positions to skip from the head (default 0)
//!   FORK_TEST_LIMIT       max orchestrators to reward (default: whole pool)
//!
//! A full run takes ~30 min on the public RPC, which may prune the fork block's state
//! before it finishes ("historical state ... is not available"). Use an archive RPC, or
//! split the run with FORK_TEST_SKIP / FORK_TEST_LIMIT.
//!
//! Locally mined blocks report the L2 block number, which RoundsManager reads as a far-future
//! round. So the test mines to a round boundary and initializes that round: every active
//! orchestrator can then call reward, covering every position in the pool.
//!
//! For each orchestrator, in pool order, it snapshots, measures `reward()` gas, reverts, then
//! computes hints with `compute_reward_hints` and sends `rewardWithHint`. It asserts that
//! - the stake increase equals `expected_reward` exactly, and
//! - the orchestrator ends up exactly between the hinted neighbours, which only happens
//!   without a search if the hints were a valid insert position.

use std::{
    env,
    time::{Duration, Instant},
};

use alloy::{
    node_bindings::Anvil,
    primitives::{Address, U256, address},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::json_rpc::{RpcRecv, RpcSend},
};
use livepeer_funds_transfer::{
    contracts::{BondingManager, BondingManagerInstance, RoundsManager},
    reward_hints::compute_reward_hints,
};

const BONDING_MANAGER: Address = address!("0x35Bcf3c30594191d53231E4FF333E8A770453e40");
const ROUNDS_MANAGER: Address = address!("0xdd6f56DcC28D3F5f27084381fE8Df634985cc39f");

/// Anvil mines each tx as soon as it is sent, so a receipt this late is never coming (for
/// example, a tx stuck in the queue behind a nonce gap). Fail instead of polling forever.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Unsigned sends (`eth_sendTransaction`) from impersonated accounts.
type Client = DynProvider;

/// A raw anvil/evm JSON-RPC call (impersonation, mining, snapshots).
async fn anvil_rpc<P: RpcSend, R: RpcRecv>(
    provider: &Client,
    method: &'static str,
    params: P,
) -> R {
    provider.raw_request(method.into(), params).await.unwrap()
}

async fn unlock(provider: &Client, who: Address) {
    let () = anvil_rpc(provider, "anvil_impersonateAccount", (who,)).await;
    let () = anvil_rpc(
        provider,
        "anvil_setBalance",
        (who, U256::from(10u64).pow(U256::from(18u64))),
    )
    .await;
}

async fn pool_order(bonding: &BondingManagerInstance<Client>) -> Vec<Address> {
    let mut out = vec![];
    let mut id = bonding.getFirstTranscoderInPool().call().await.unwrap();
    while !id.is_zero() {
        out.push(id);
        id = bonding.getNextTranscoderInPool(id).call().await.unwrap();
    }
    out
}

#[tokio::test]
#[ignore = "forks Arbitrum mainnet with anvil; run with --ignored"]
async fn reward_with_hint_lands_at_hinted_position_on_fork() {
    let rpc = env::var("ARB_RPC_URL").unwrap_or_else(|_| "https://arb1.arbitrum.io/rpc".into());
    let anvil = Anvil::new().fork(rpc).timeout(120_000).spawn();
    // Not the recommended fillers: their cached nonce manager keeps counting locally, so after
    // evm_revert drops the baseline reward() tx it hands out a nonce one too high and the
    // rewardWithHint tx waits in anvil's queue forever. Ask the node for the nonce every time.
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .with_gas_estimation()
        .with_simple_nonce_management()
        .connect_http(anvil.endpoint_url())
        .erased();

    let bonding = BondingManager::new(BONDING_MANAGER, provider.clone());
    let rounds = RoundsManager::new(ROUNDS_MANAGER, provider.clone());

    // Move to the start of a fresh round and initialize it.
    let round_length = rounds.roundLength().call().await.unwrap();
    let block = U256::from(provider.get_block_number().await.unwrap());
    let to_boundary = round_length - block % round_length;
    let () = anvil_rpc(&provider, "anvil_mine", (to_boundary,)).await;
    let orchestrators = pool_order(&bonding).await;
    let initializer = orchestrators[0];
    unlock(&provider, initializer).await;
    rounds
        .initializeRound()
        .from(initializer)
        .send()
        .await
        .unwrap()
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .unwrap();
    let current_round = rounds.currentRound().call().await.unwrap();
    assert!(rounds.currentRoundInitialized().call().await.unwrap());

    let skip: usize = env::var("FORK_TEST_SKIP")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(0);
    let limit = env::var("FORK_TEST_LIMIT")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(orchestrators.len());
    println!(
        "round {current_round}: rewarding up to {limit} of {} orchestrators from position {skip}",
        orchestrators.len()
    );

    let (mut gas_plain, mut gas_hinted) = (0u64, 0u64);
    let started = Instant::now();

    for (i, &orch) in orchestrators.iter().enumerate().skip(skip).take(limit) {
        unlock(&provider, orch).await;
        if !bonding.isActiveTranscoder(orch).call().await.unwrap() {
            println!("{i:>3} {orch:?} not active this round, skipping");
            continue;
        }

        // Baseline: plain reward() gas, then roll back.
        let snapshot: U256 = anvil_rpc(&provider, "evm_snapshot", ()).await;
        let plain = bonding
            .reward()
            .from(orch)
            .send()
            .await
            .unwrap()
            .with_timeout(Some(RECEIPT_TIMEOUT))
            .get_receipt()
            .await
            .unwrap();
        assert!(plain.status(), "reward failed for {orch:?}");
        let reverted: bool = anvil_rpc(&provider, "evm_revert", (snapshot,)).await;
        assert!(reverted);

        let before = bonding.transcoderTotalStake(orch).call().await.unwrap();
        let hints = compute_reward_hints(&bonding, orch, current_round)
            .await
            .unwrap();
        assert_eq!(hints.current_stake, before);

        let receipt = bonding
            .rewardWithHint(hints.prev, hints.next)
            .from(orch)
            .send()
            .await
            .unwrap()
            .with_timeout(Some(RECEIPT_TIMEOUT))
            .get_receipt()
            .await
            .unwrap();
        assert!(receipt.status(), "rewardWithHint failed for {orch:?}");

        let after = bonding.transcoderTotalStake(orch).call().await.unwrap();
        assert_eq!(
            after - before,
            hints.expected_reward,
            "reward amount mismatch for {orch:?}"
        );

        // The orchestrator must sit exactly between the hinted neighbours.
        let actual_prev_next = if hints.prev.is_zero() {
            bonding.getFirstTranscoderInPool().call().await.unwrap()
        } else {
            bonding
                .getNextTranscoderInPool(hints.prev)
                .call()
                .await
                .unwrap()
        };
        let actual_next = bonding.getNextTranscoderInPool(orch).call().await.unwrap();
        assert_eq!(
            actual_prev_next, orch,
            "{orch:?} not after hinted prev {:?}",
            hints.prev
        );
        assert_eq!(actual_next, hints.next, "{orch:?} not before hinted next");

        let plain_gas = plain.gas_used;
        let hinted_gas = receipt.gas_used;
        gas_plain += plain_gas;
        gas_hinted += hinted_gas;
        println!(
            "{i:>3} {orch:?} reward={:>10}gas withHint={:>10}gas prev={:?} next={:?}",
            plain_gas, hinted_gas, hints.prev, hints.next
        );
    }

    println!(
        "total gas: reward()={gas_plain} rewardWithHint()={gas_hinted} ({:.1}% saved) in {:?}",
        100.0 * (gas_plain - gas_hinted) as f64 / gas_plain as f64,
        started.elapsed()
    );
    assert!(gas_hinted < gas_plain);
}
