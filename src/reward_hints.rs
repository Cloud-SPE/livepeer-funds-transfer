//! List hints for `BondingManager.rewardWithHint(_newPosPrev, _newPosNext)`.
//!
//! The transcoder pool is a SortedDoublyLL keyed by total stake (highest first). Calling
//! reward re-inserts the caller with its new stake; without hints the contract walks the
//! list from the head to find the spot. Correct neighbours let it skip that walk. Wrong or
//! stale hints never revert: the contract falls back to searching from them.
//!
//! The math mirrors the deployed contracts (livepeer/protocol `delta` branch):
//! - `BondingManager._rewardWithHint` / `increaseTotalStake`
//! - `Minter.createReward`, which uses `MathUtilsV2` (PERC_DIVISOR = 1e9)
//! - `PreciseMathUtils` for the treasury cut (PERC_DIVISOR = 1e27)
//! - `SortedDoublyLL.validInsertPosition`

use std::fmt;

use alloy::{
    contract::Error as ContractError,
    primitives::{Address, U256, keccak256},
    providers::Provider,
};

use crate::contracts::{BondingManagerInstance, Controller, Minter};

/// MathUtilsV2.PERC_DIVISOR, used by Minter.createReward.
const MINTER_PERC_DIVISOR: u64 = 1_000_000_000;

/// PreciseMathUtils.PERC_DIVISOR, used for treasuryRewardCutRate.
fn precise_perc_divisor() -> U256 {
    U256::from(10u64).pow(U256::from(27u64))
}

/// On-chain values that determine the reward minted for a transcoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewardInputs {
    /// Minter.currentMintableTokens()
    pub mintable_tokens: U256,
    /// EarningsPool.totalStake that _rewardWithHint uses (see [`earnings_pool_round`]).
    pub earnings_pool_stake: U256,
    /// BondingManager.currentRoundTotalActiveStake()
    pub total_active_stake: U256,
    /// BondingManager.treasuryRewardCutRate(), out of 1e27.
    pub treasury_reward_cut_rate: U256,
}

/// Stake the transcoder gains from reward(): Minter.createReward minus the treasury cut.
pub fn expected_transcoder_reward(inputs: &RewardInputs) -> U256 {
    if inputs.total_active_stake.is_zero() {
        return U256::ZERO;
    }
    let divisor = U256::from(MINTER_PERC_DIVISOR);
    // MathUtilsV2.percOf(amount, num, denom) = amount * (num * D / denom) / D
    let perc_points = inputs.earnings_pool_stake * divisor / inputs.total_active_stake;
    let total = inputs.mintable_tokens * perc_points / divisor;
    let treasury = total * inputs.treasury_reward_cut_rate / precise_perc_divisor();
    total - treasury
}

/// Round whose EarningsPool.totalStake _rewardWithHint uses for the reward amount.
///
/// If the transcoder's stake was last updated before the current round, the contract syncs
/// the current pool's stake from `lastActiveStakeUpdateRound`; otherwise it uses the current pool.
pub fn earnings_pool_round(last_active_stake_update_round: U256, current_round: U256) -> U256 {
    std::cmp::min(last_active_stake_update_round, current_round)
}

/// Read access to the transcoder pool linked list.
#[allow(async_fn_in_trait)]
pub trait PoolSource {
    type Error;
    /// Head of the list (highest stake), or zero if empty.
    async fn first(&self) -> Result<Address, Self::Error>;
    /// Node after `id`, or zero at the tail.
    async fn next(&self, id: Address) -> Result<Address, Self::Error>;
    /// List key of `id`, i.e. its total stake.
    async fn stake(&self, id: Address) -> Result<U256, Self::Error>;
}

impl<P: Provider> PoolSource for BondingManagerInstance<P> {
    type Error = ContractError;

    async fn first(&self) -> Result<Address, Self::Error> {
        self.getFirstTranscoderInPool().call().await
    }

    async fn next(&self, id: Address) -> Result<Address, Self::Error> {
        self.getNextTranscoderInPool(id).call().await
    }

    async fn stake(&self, id: Address) -> Result<U256, Self::Error> {
        self.transcoderTotalStake(id).call().await
    }
}

#[derive(Debug)]
pub enum HintError<E> {
    Source(E),
    /// The walk visited more nodes than the pool can hold (corrupt or changing list).
    PoolTooLong(usize),
}

impl<E: fmt::Display> fmt::Display for HintError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HintError::Source(e) => write!(f, "{e}"),
            HintError::PoolTooLong(n) => write!(f, "pool walk exceeded {n} nodes"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for HintError<E> {}

impl<E> From<E> for HintError<E> {
    fn from(e: E) -> Self {
        HintError::Source(e)
    }
}

/// Finds `(prev, next)` for re-inserting `me` with `new_stake`.
///
/// Walks from the head, skipping `me` (updateKey removes it before inserting), and stops at
/// the first node with stake below `new_stake`. The result satisfies
/// SortedDoublyLL.validInsertPosition: `prev.next == next` and
/// `stake(prev) >= new_stake >= stake(next)`, with zero meaning head/tail.
pub async fn find_insert_hints<P: PoolSource>(
    pool: &P,
    me: Address,
    new_stake: U256,
    max_nodes: usize,
) -> Result<(Address, Address), HintError<P::Error>> {
    let mut prev = Address::ZERO;
    let mut id = pool.first().await?;
    let mut visited = 0usize;

    while !id.is_zero() {
        visited += 1;
        if visited > max_nodes {
            return Err(HintError::PoolTooLong(max_nodes));
        }
        if id != me {
            if pool.stake(id).await? < new_stake {
                return Ok((prev, id));
            }
            prev = id;
        }
        id = pool.next(id).await?;
    }

    Ok((prev, Address::ZERO))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewardHints {
    pub prev: Address,
    pub next: Address,
    pub current_stake: U256,
    pub expected_reward: U256,
}

/// Reads the reward inputs and pool from chain and computes hints for `transcoder`.
pub async fn compute_reward_hints<P: Provider>(
    bonding: &BondingManagerInstance<P>,
    transcoder: Address,
    current_round: U256,
) -> Result<RewardHints, HintError<ContractError>> {
    let provider = bonding.provider();
    // Resolve the Minter through the Controller each time; it can be upgraded.
    let controller = Controller::new(bonding.controller().call().await?, provider);
    let minter_addr = controller.getContract(keccak256("Minter")).call().await?;
    let minter = Minter::new(minter_addr, provider);

    let t = bonding.getTranscoder(transcoder).call().await?;
    let pool_round = earnings_pool_round(t.lastActiveStakeUpdateRound, current_round);
    let earnings_pool = bonding
        .getTranscoderEarningsPoolForRound(transcoder, pool_round)
        .call()
        .await?;

    let inputs = RewardInputs {
        mintable_tokens: minter.currentMintableTokens().call().await?,
        earnings_pool_stake: earnings_pool.totalStake,
        total_active_stake: bonding.currentRoundTotalActiveStake().call().await?,
        treasury_reward_cut_rate: bonding.treasuryRewardCutRate().call().await?,
    };
    let expected_reward = expected_transcoder_reward(&inputs);

    let current_stake = bonding.transcoderTotalStake(transcoder).call().await?;
    let max_nodes = bonding.getTranscoderPoolMaxSize().call().await?;
    let max_nodes = max_nodes.saturating_to::<usize>();

    let (prev, next) = find_insert_hints(
        bonding,
        transcoder,
        current_stake + expected_reward,
        max_nodes,
    )
    .await?;

    Ok(RewardHints {
        prev,
        next,
        current_stake,
        expected_reward,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, convert::Infallible, future::Future};

    use proptest::prelude::*;

    use super::*;

    fn addr(n: u64) -> Address {
        Address::left_padding_from(&n.to_be_bytes())
    }

    fn wei(s: &str) -> U256 {
        U256::from_str_radix(s, 10).unwrap()
    }

    fn u(n: u64) -> U256 {
        U256::from(n)
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// In-memory pool in list order (highest stake first).
    struct VecPool {
        order: Vec<Address>,
        stakes: HashMap<Address, U256>,
    }

    impl VecPool {
        fn new(nodes: &[(Address, U256)]) -> Self {
            Self {
                order: nodes.iter().map(|(a, _)| *a).collect(),
                stakes: nodes.iter().copied().collect(),
            }
        }
    }

    impl PoolSource for VecPool {
        type Error = Infallible;

        async fn first(&self) -> Result<Address, Infallible> {
            Ok(self.order.first().copied().unwrap_or_default())
        }

        async fn next(&self, id: Address) -> Result<Address, Infallible> {
            let i = self.order.iter().position(|a| *a == id).unwrap();
            Ok(self.order.get(i + 1).copied().unwrap_or_default())
        }

        async fn stake(&self, id: Address) -> Result<U256, Infallible> {
            Ok(self.stakes[&id])
        }
    }

    /// Port of SortedDoublyLL.validInsertPosition, evaluated on the list with `me` removed
    /// (updateKey calls remove() before insert()).
    fn valid_insert_position(
        nodes: &[(Address, U256)],
        me: Address,
        key: U256,
        prev: Address,
        next: Address,
    ) -> bool {
        let list: Vec<_> = nodes.iter().copied().filter(|(a, _)| *a != me).collect();
        let pos = |id: Address| list.iter().position(|(a, _)| *a == id);
        let key_of = |id: Address| list[pos(id).unwrap()].1;
        let zero = Address::ZERO;

        if prev == zero && next == zero {
            list.is_empty()
        } else if prev == zero {
            list.first().map(|(a, _)| *a) == Some(next) && key >= key_of(next)
        } else if next == zero {
            list.last().map(|(a, _)| *a) == Some(prev) && key <= key_of(prev)
        } else {
            match (pos(prev), pos(next)) {
                (Some(p), Some(n)) => n == p + 1 && key_of(prev) >= key && key >= key_of(next),
                _ => false,
            }
        }
    }

    fn hints(nodes: &[(Address, U256)], me: Address, new_stake: U256) -> (Address, Address) {
        block_on(find_insert_hints(&VecPool::new(nodes), me, new_stake, 100)).unwrap()
    }

    fn pool(stakes: &[u64]) -> Vec<(Address, U256)> {
        stakes
            .iter()
            .enumerate()
            .map(|(i, s)| (addr(i as u64 + 1), U256::from(*s)))
            .collect()
    }

    // ---------------------------------------------------------------
    // Reward amount
    // ---------------------------------------------------------------

    /// Mainnet round 4371: currentMintableTokens and currentRoundTotalActiveStake.
    const R4371_MINTABLE: &str = "29187141366064491809863";
    const R4371_TOTAL_ACTIVE: &str = "30056420251363377765842750";

    #[test]
    fn reward_matches_mainnet_reward_events() {
        // (EarningsPool(4371).totalStake, Reward event amount) for three orchestrators.
        let cases = [
            // 0xd21ee13175e0cf56876e76b0fa4003cd19e9ad2e
            ("8251085550154983237942", "8012424860670658227"),
            // 0xac2e50c8f7ac0f82923a7df9d9903f6ec4741919
            ("4153152832095735345431558", "4033037106104941613289"),
            // 0x5263e0ce3a97b634d8828ce4337ad0f70b30b077
            ("113245605417987738597544", "109970348063392712112"),
        ];
        for (pool_stake, event_amount) in cases {
            let inputs = RewardInputs {
                mintable_tokens: wei(R4371_MINTABLE),
                earnings_pool_stake: wei(pool_stake),
                total_active_stake: wei(R4371_TOTAL_ACTIVE),
                treasury_reward_cut_rate: U256::ZERO,
            };
            assert_eq!(expected_transcoder_reward(&inputs), wei(event_amount));
        }
    }

    #[test]
    fn reward_truncates_like_the_contract_not_exact_division() {
        // Exact division gives 26334841938074 wei more than the contract mints here.
        let inputs = RewardInputs {
            mintable_tokens: wei(R4371_MINTABLE),
            earnings_pool_stake: wei("8251085550154983237942"),
            total_active_stake: wei(R4371_TOTAL_ACTIVE),
            treasury_reward_cut_rate: U256::ZERO,
        };
        let exact = inputs.mintable_tokens * inputs.earnings_pool_stake / inputs.total_active_stake;
        assert_eq!(
            exact - expected_transcoder_reward(&inputs),
            wei("26334841938074")
        );
    }

    #[test]
    fn reward_subtracts_treasury_cut() {
        // 10% cut (1e26 / 1e27). Minted = 1000 * (250e9 / 1000) / 1e9 = 250, treasury = 25.
        let inputs = RewardInputs {
            mintable_tokens: u(1000),
            earnings_pool_stake: u(250),
            total_active_stake: u(1000),
            treasury_reward_cut_rate: U256::from(10u64).pow(u(26)),
        };
        assert_eq!(expected_transcoder_reward(&inputs), u(225));
    }

    #[test]
    fn reward_is_zero_without_active_stake() {
        let inputs = RewardInputs {
            mintable_tokens: u(1000),
            earnings_pool_stake: u(250),
            total_active_stake: U256::ZERO,
            treasury_reward_cut_rate: U256::ZERO,
        };
        assert_eq!(expected_transcoder_reward(&inputs), U256::ZERO);
    }

    #[test]
    fn earnings_pool_round_uses_last_update_when_stale() {
        assert_eq!(earnings_pool_round(u(4300), u(4371)), U256::from(4300));
        assert_eq!(earnings_pool_round(u(4371), u(4371)), U256::from(4371));
        // Stake changed this round: lastActiveStakeUpdateRound is already current + 1.
        assert_eq!(earnings_pool_round(u(4372), u(4371)), U256::from(4371));
    }

    // ---------------------------------------------------------------
    // Insert position
    // ---------------------------------------------------------------

    #[test]
    fn hints_stay_in_place_when_reward_does_not_pass_anyone() {
        let p = pool(&[500, 400, 300, 200]);
        // addr(3) goes 300 -> 310: still between 400 and 200.
        assert_eq!(hints(&p, addr(3), u(310)), (addr(2), addr(4)));
    }

    #[test]
    fn hints_move_up_when_reward_passes_a_neighbour() {
        let p = pool(&[500, 400, 300, 200]);
        assert_eq!(hints(&p, addr(3), u(450)), (addr(1), addr(2)));
    }

    #[test]
    fn hints_for_new_head() {
        let p = pool(&[500, 400, 300]);
        assert_eq!(hints(&p, addr(3), u(600)), (Address::ZERO, addr(1)));
        // Already the head and staying there.
        assert_eq!(hints(&p, addr(1), u(510)), (Address::ZERO, addr(2)));
    }

    #[test]
    fn hints_for_tail() {
        let p = pool(&[500, 400, 300]);
        assert_eq!(hints(&p, addr(3), u(301)), (addr(2), Address::ZERO));
    }

    #[test]
    fn hints_for_only_member() {
        let p = pool(&[500]);
        assert_eq!(hints(&p, addr(1), u(600)), (Address::ZERO, Address::ZERO));
    }

    #[test]
    fn hints_with_equal_stake_go_after_the_tie() {
        let p = pool(&[500, 400, 400, 100]);
        let (prev, next) = hints(&p, addr(4), u(400));
        assert_eq!((prev, next), (addr(3), Address::ZERO));
        assert!(valid_insert_position(&p, addr(4), u(400), prev, next));
    }

    #[test]
    fn walk_stops_at_insert_point() {
        struct Counting<'a>(&'a VecPool, std::cell::Cell<usize>);
        impl PoolSource for Counting<'_> {
            type Error = Infallible;
            async fn first(&self) -> Result<Address, Infallible> {
                self.0.first().await
            }
            async fn next(&self, id: Address) -> Result<Address, Infallible> {
                self.0.next(id).await
            }
            async fn stake(&self, id: Address) -> Result<U256, Infallible> {
                self.1.set(self.1.get() + 1);
                self.0.stake(id).await
            }
        }
        let p = VecPool::new(&pool(&[500, 400, 300, 200, 100, 50]));
        let counting = Counting(&p, Default::default());
        block_on(find_insert_hints(&counting, addr(2), u(410), 100)).unwrap();
        // Reads 500 then 300 (skipping itself) and stops.
        assert_eq!(counting.1.get(), 2);
    }

    #[test]
    fn walk_errors_past_max_nodes() {
        let p = VecPool::new(&pool(&[500, 400, 300]));
        let err = block_on(find_insert_hints(&p, addr(9), u(1), 2)).unwrap_err();
        assert!(matches!(err, HintError::PoolTooLong(2)));
    }

    /// Random pools sorted by stake (descending) with unique addresses. Small stake ranges
    /// make ties common.
    fn arb_pool() -> impl Strategy<Value = Vec<(Address, U256)>> {
        prop_oneof![
            prop::collection::vec(1u64..20, 1..100),
            prop::collection::vec(1u64..u64::MAX, 1..100),
        ]
        .prop_map(|mut stakes| {
            stakes.sort_unstable_by(|a, b| b.cmp(a));
            pool(&stakes)
        })
    }

    proptest! {
        #[test]
        fn hints_are_always_a_valid_insert_position(
            nodes in arb_pool(),
            me_index in any::<prop::sample::Index>(),
            reward in prop_oneof![0u64..5, 0u64..u64::MAX],
        ) {
            let (me, stake) = nodes[me_index.index(nodes.len())];
            let new_stake = stake + U256::from(reward);
            let (prev, next) = hints(&nodes, me, new_stake);
            prop_assert!(
                valid_insert_position(&nodes, me, new_stake, prev, next),
                "invalid hints prev={prev:?} next={next:?} new_stake={new_stake}"
            );
        }

        #[test]
        fn hints_never_point_at_self(
            nodes in arb_pool(),
            me_index in any::<prop::sample::Index>(),
            reward in 0u64..u64::MAX,
        ) {
            let (me, stake) = nodes[me_index.index(nodes.len())];
            let (prev, next) = hints(&nodes, me, stake + U256::from(reward));
            prop_assert_ne!(prev, me);
            prop_assert_ne!(next, me);
        }
    }
}

/// `compute_reward_hints` against a scripted JSON-RPC transport.
#[cfg(test)]
mod rpc_tests {
    use alloy::{primitives::Bytes, providers::RootProvider, sol_types::SolValue};

    use super::*;
    use crate::{contracts::BondingManager, mock_rpc::MockRpc};

    type Bonding = BondingManagerInstance<RootProvider>;

    const BONDING: u64 = 0xb0;
    const CONTROLLER: u64 = 0xc0;
    const MINTER: u64 = 0xd0;
    const ME: u64 = 3;

    fn addr(n: u64) -> Address {
        Address::left_padding_from(&n.to_be_bytes())
    }

    fn u(n: u64) -> U256 {
        U256::from(n)
    }

    fn setup() -> (Bonding, MockRpc) {
        let mock = MockRpc::new();
        let provider = RootProvider::new(mock.client());
        (BondingManager::new(addr(BONDING), provider), mock)
    }

    fn enc<T: SolValue>(value: T) -> Bytes {
        value.abi_encode().into()
    }

    /// eth_call results for the nine reads before the pool walk, up to and including
    /// getTranscoderPoolMaxSize. EarningsPool.totalStake 250 of 1000 active, 1000 mintable,
    /// 10% treasury cut: reward = 250 - 25 = 225. Current stake is 300.
    fn inputs(last_active_round: u64, max_pool: U256) -> Vec<Bytes> {
        let mut t = [U256::ZERO; 10];
        t[0] = u(4370);
        t[3] = u(last_active_round);
        vec![
            enc(addr(CONTROLLER)),
            enc(addr(MINTER)),
            enc(t),
            enc([u(250), U256::ZERO, U256::ZERO, U256::ZERO, U256::ZERO]),
            enc(u(1000)),
            enc(u(1000)),
            enc(U256::from(10u64).pow(u(26))),
            enc(u(300)),
            enc(max_pool),
        ]
    }

    fn script(mock: &MockRpc, results: &[Bytes]) {
        for r in results {
            mock.ok("eth_call", r);
        }
    }

    /// Asserts the nine input reads, with the earnings pool read for `pool_round`.
    fn expect_inputs(bonding: &Bonding, mock: &MockRpc, pool_round: u64) {
        let provider = bonding.provider();
        let controller = Controller::new(addr(CONTROLLER), provider);
        let minter = Minter::new(addr(MINTER), provider);
        mock.expect_call(&bonding.controller());
        mock.expect_call(&controller.getContract(keccak256("Minter")));
        mock.expect_call(&bonding.getTranscoder(addr(ME)));
        mock.expect_call(&bonding.getTranscoderEarningsPoolForRound(addr(ME), u(pool_round)));
        mock.expect_call(&minter.currentMintableTokens());
        mock.expect_call(&bonding.currentRoundTotalActiveStake());
        mock.expect_call(&bonding.treasuryRewardCutRate());
        mock.expect_call(&bonding.transcoderTotalStake(addr(ME)));
        mock.expect_call(&bonding.getTranscoderPoolMaxSize());
    }

    #[tokio::test]
    async fn computes_hints_from_chain_state() {
        let (bonding, mock) = setup();
        // Pool: 1=500, 2=400, me=300, 4=200. 300 + 225 = 525 passes everyone.
        script(&mock, &inputs(4300, u(100)));
        mock.ret(addr(1));
        mock.ret(u(500));

        let hints = compute_reward_hints(&bonding, addr(ME), u(4371))
            .await
            .unwrap();
        assert_eq!(
            hints,
            RewardHints {
                prev: Address::ZERO,
                next: addr(1),
                current_stake: u(300),
                expected_reward: u(225),
            }
        );

        // The Minter is resolved through the Controller the BondingManager reports, and the
        // earnings pool is read for lastActiveStakeUpdateRound (stale stake).
        expect_inputs(&bonding, &mock, 4300);
        mock.expect_call(&bonding.getFirstTranscoderInPool());
        mock.expect_call(&bonding.transcoderTotalStake(addr(1)));
        mock.expect_no_more_requests();
    }

    #[tokio::test]
    async fn uses_current_round_pool_when_stake_updated_this_round() {
        let (bonding, mock) = setup();
        // lastActiveStakeUpdateRound = current + 1 -> pool for the current round.
        script(&mock, &inputs(4372, u(100)));
        // Walk: 1=600 stays ahead, me skipped, 4=200 is below 525.
        mock.ret(addr(1));
        mock.ret(u(600));
        mock.ret(addr(ME));
        mock.ret(addr(4));
        mock.ret(u(200));

        let hints = compute_reward_hints(&bonding, addr(ME), u(4371))
            .await
            .unwrap();
        assert_eq!((hints.prev, hints.next), (addr(1), addr(4)));

        expect_inputs(&bonding, &mock, 4371);
        mock.expect_call(&bonding.getFirstTranscoderInPool());
        mock.expect_call(&bonding.transcoderTotalStake(addr(1)));
        mock.expect_call(&bonding.getNextTranscoderInPool(addr(1)));
        mock.expect_call(&bonding.getNextTranscoderInPool(addr(ME)));
        mock.expect_call(&bonding.transcoderTotalStake(addr(4)));
        mock.expect_no_more_requests();
    }

    #[tokio::test]
    async fn empty_pool_gives_zero_hints() {
        let (bonding, mock) = setup();
        script(&mock, &inputs(4371, u(100)));
        mock.ret(Address::ZERO);

        let hints = compute_reward_hints(&bonding, addr(ME), u(4371))
            .await
            .unwrap();
        assert_eq!((hints.prev, hints.next), (Address::ZERO, Address::ZERO));
        assert_eq!(hints.expected_reward, u(225));
    }

    #[tokio::test]
    async fn pool_max_size_bounds_the_walk() {
        let (bonding, mock) = setup();
        script(&mock, &inputs(4371, U256::ZERO));
        mock.ret(addr(1));

        let err = compute_reward_hints(&bonding, addr(ME), u(4371))
            .await
            .unwrap_err();
        assert!(matches!(err, HintError::PoolTooLong(0)), "{err}");
        assert_eq!(err.to_string(), "pool walk exceeded 0 nodes");
    }

    #[tokio::test]
    async fn oversized_pool_max_size_saturates() {
        // 2^64 must not truncate to a 0-node bound (which would fail the walk).
        let (bonding, mock) = setup();
        script(&mock, &inputs(4371, U256::from(1u64) << 64));
        mock.ret(addr(1));
        mock.ret(u(100));
        mock.ret(Address::ZERO);

        let hints = compute_reward_hints(&bonding, addr(ME), u(4371))
            .await
            .unwrap();
        assert_eq!((hints.prev, hints.next), (Address::ZERO, addr(1)));
    }

    #[tokio::test]
    async fn rpc_errors_are_returned_at_every_step() {
        // Fail each of the nine input reads, plus the first pool read.
        for fail_at in 0..10 {
            let (bonding, mock) = setup();
            let mut results = inputs(4371, u(100));
            results.push(enc(addr(1)));
            script(&mock, &results[..fail_at]);
            mock.err("eth_call");

            let err = compute_reward_hints(&bonding, addr(ME), u(4371))
                .await
                .unwrap_err();
            assert!(matches!(err, HintError::Source(_)), "step {fail_at}: {err}");
            assert!(err.to_string().contains("boom"), "step {fail_at}: {err}");
        }
    }

    #[tokio::test]
    async fn malformed_return_data_is_an_error() {
        let (bonding, mock) = setup();
        // controller() returns no data.
        mock.ret_empty();
        let err = compute_reward_hints(&bonding, addr(ME), u(4371))
            .await
            .unwrap_err();
        assert!(matches!(err, HintError::Source(_)));
    }
}
