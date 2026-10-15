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

use std::{env, sync::Arc, time::Instant};

use ethers::{
    providers::{Http, Middleware, Provider},
    types::{Address, U256},
    utils::Anvil,
};
use livepeer_funds_transfer::{
    contracts::{BondingManager, RoundsManager},
    reward_hints::compute_reward_hints,
};

const BONDING_MANAGER: &str = "0x35Bcf3c30594191d53231E4FF333E8A770453e40";
const ROUNDS_MANAGER: &str = "0xdd6f56DcC28D3F5f27084381fE8Df634985cc39f";

type Client = Provider<Http>;

async fn unlock(provider: &Client, who: Address) {
    provider
        .request::<_, ()>("anvil_impersonateAccount", [who])
        .await
        .unwrap();
    provider
        .request::<_, ()>("anvil_setBalance", (who, U256::exp10(18)))
        .await
        .unwrap();
}

async fn pool_order(bonding: &BondingManager<Client>) -> Vec<Address> {
    let mut out = vec![];
    let mut id = bonding.get_first_transcoder_in_pool().call().await.unwrap();
    while !id.is_zero() {
        out.push(id);
        id = bonding
            .get_next_transcoder_in_pool(id)
            .call()
            .await
            .unwrap();
    }
    out
}

#[tokio::test]
#[ignore = "forks Arbitrum mainnet with anvil; run with --ignored"]
async fn reward_with_hint_lands_at_hinted_position_on_fork() {
    let rpc = env::var("ARB_RPC_URL").unwrap_or_else(|_| "https://arb1.arbitrum.io/rpc".into());
    let anvil = Anvil::new().fork(rpc).timeout(120_000u64).spawn();
    let provider = Arc::new(Provider::<Http>::try_from(anvil.endpoint()).unwrap());

    let bonding = BondingManager::new(
        BONDING_MANAGER.parse::<Address>().unwrap(),
        provider.clone(),
    );
    let rounds = RoundsManager::new(ROUNDS_MANAGER.parse::<Address>().unwrap(), provider.clone());

    // Move to the start of a fresh round and initialize it.
    let round_length = rounds.round_length().call().await.unwrap();
    let block = U256::from(provider.get_block_number().await.unwrap().as_u64());
    let to_boundary = round_length - block % round_length;
    provider
        .request::<_, ()>("anvil_mine", [to_boundary])
        .await
        .unwrap();
    let orchestrators = pool_order(&bonding).await;
    let initializer = orchestrators[0];
    unlock(&provider, initializer).await;
    rounds
        .initialize_round()
        .from(initializer)
        .send()
        .await
        .unwrap()
        .await
        .unwrap();
    let current_round = rounds.current_round().call().await.unwrap();
    assert!(rounds.current_round_initialized().call().await.unwrap());

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

    let (mut gas_plain, mut gas_hinted) = (U256::zero(), U256::zero());
    let started = Instant::now();

    for (i, &orch) in orchestrators.iter().enumerate().skip(skip).take(limit) {
        unlock(&provider, orch).await;
        if !bonding.is_active_transcoder(orch).call().await.unwrap() {
            println!("{i:>3} {orch:?} not active this round, skipping");
            continue;
        }

        // Baseline: plain reward() gas, then roll back.
        let snapshot: U256 = provider.request("evm_snapshot", ()).await.unwrap();
        let plain = bonding
            .reward()
            .from(orch)
            .send()
            .await
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        let reverted: bool = provider.request("evm_revert", [snapshot]).await.unwrap();
        assert!(reverted);

        let before = bonding.transcoder_total_stake(orch).call().await.unwrap();
        let hints = compute_reward_hints(&bonding, orch, current_round)
            .await
            .unwrap();
        assert_eq!(hints.current_stake, before);

        let receipt = bonding
            .reward_with_hint(hints.prev, hints.next)
            .from(orch)
            .send()
            .await
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.status,
            Some(1.into()),
            "rewardWithHint failed for {orch:?}"
        );

        let after = bonding.transcoder_total_stake(orch).call().await.unwrap();
        assert_eq!(
            after - before,
            hints.expected_reward,
            "reward amount mismatch for {orch:?}"
        );

        // The orchestrator must sit exactly between the hinted neighbours.
        let actual_prev_next = if hints.prev.is_zero() {
            bonding.get_first_transcoder_in_pool().call().await.unwrap()
        } else {
            bonding
                .get_next_transcoder_in_pool(hints.prev)
                .call()
                .await
                .unwrap()
        };
        let actual_next = bonding
            .get_next_transcoder_in_pool(orch)
            .call()
            .await
            .unwrap();
        assert_eq!(
            actual_prev_next, orch,
            "{orch:?} not after hinted prev {:?}",
            hints.prev
        );
        assert_eq!(actual_next, hints.next, "{orch:?} not before hinted next");

        let plain_gas = plain.gas_used.unwrap();
        let hinted_gas = receipt.gas_used.unwrap();
        gas_plain += plain_gas;
        gas_hinted += hinted_gas;
        println!(
            "{i:>3} {orch:?} reward={:>10}gas withHint={:>10}gas prev={:?} next={:?}",
            plain_gas, hinted_gas, hints.prev, hints.next
        );
    }

    println!(
        "total gas: reward()={gas_plain} rewardWithHint()={gas_hinted} ({:.1}% saved) in {:?}",
        100.0 * (gas_plain - gas_hinted).as_u128() as f64 / gas_plain.as_u128() as f64,
        started.elapsed()
    );
    assert!(gas_hinted < gas_plain);
}
