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

use ethers::{
    contract::ContractError,
    providers::Middleware,
    types::{Address, U256},
    utils::keccak256,
};

use crate::contracts::{BondingManager, Controller, Minter};

/// MathUtilsV2.PERC_DIVISOR, used by Minter.createReward.
const MINTER_PERC_DIVISOR: u64 = 1_000_000_000;

/// PreciseMathUtils.PERC_DIVISOR, used for treasuryRewardCutRate.
fn precise_perc_divisor() -> U256 {
    U256::exp10(27)
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
        return U256::zero();
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

impl<M: Middleware> PoolSource for BondingManager<M> {
    type Error = ContractError<M>;

    async fn first(&self) -> Result<Address, Self::Error> {
        self.get_first_transcoder_in_pool().call().await
    }

    async fn next(&self, id: Address) -> Result<Address, Self::Error> {
        self.get_next_transcoder_in_pool(id).call().await
    }

    async fn stake(&self, id: Address) -> Result<U256, Self::Error> {
        self.transcoder_total_stake(id).call().await
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
    let mut prev = Address::zero();
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

    Ok((prev, Address::zero()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewardHints {
    pub prev: Address,
    pub next: Address,
    pub current_stake: U256,
    pub expected_reward: U256,
}

/// Reads the reward inputs and pool from chain and computes hints for `transcoder`.
pub async fn compute_reward_hints<M: Middleware>(
    bonding: &BondingManager<M>,
    transcoder: Address,
    current_round: U256,
) -> Result<RewardHints, HintError<ContractError<M>>> {
    let client = bonding.client();
    // Resolve the Minter through the Controller each time; it can be upgraded.
    let controller = Controller::new(bonding.controller().call().await?, client.clone());
    let minter_addr = controller.get_contract(keccak256("Minter")).call().await?;
    let minter = Minter::new(minter_addr, client);

    let t = bonding.get_transcoder(transcoder).call().await?;
    let last_active_stake_update_round = t.3;
    let pool_round = earnings_pool_round(last_active_stake_update_round, current_round);
    let earnings_pool = bonding
        .get_transcoder_earnings_pool_for_round(transcoder, pool_round)
        .call()
        .await?;

    let inputs = RewardInputs {
        mintable_tokens: minter.current_mintable_tokens().call().await?,
        earnings_pool_stake: earnings_pool.0,
        total_active_stake: bonding.current_round_total_active_stake().call().await?,
        treasury_reward_cut_rate: bonding.treasury_reward_cut_rate().call().await?,
    };
    let expected_reward = expected_transcoder_reward(&inputs);

    let current_stake = bonding.transcoder_total_stake(transcoder).call().await?;
    let max_nodes = bonding.get_transcoder_pool_max_size().call().await?;
    let max_nodes = usize::try_from(max_nodes.low_u64()).unwrap_or(usize::MAX);

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
        Address::from_low_u64_be(n)
    }

    fn wei(s: &str) -> U256 {
        U256::from_dec_str(s).unwrap()
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
        let zero = Address::zero();

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
                treasury_reward_cut_rate: U256::zero(),
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
            treasury_reward_cut_rate: U256::zero(),
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
            mintable_tokens: U256::from(1000u64),
            earnings_pool_stake: U256::from(250u64),
            total_active_stake: U256::from(1000u64),
            treasury_reward_cut_rate: U256::exp10(26),
        };
        assert_eq!(expected_transcoder_reward(&inputs), U256::from(225u64));
    }

    #[test]
    fn reward_is_zero_without_active_stake() {
        let inputs = RewardInputs {
            mintable_tokens: U256::from(1000u64),
            earnings_pool_stake: U256::from(250u64),
            total_active_stake: U256::zero(),
            treasury_reward_cut_rate: U256::zero(),
        };
        assert_eq!(expected_transcoder_reward(&inputs), U256::zero());
    }

    #[test]
    fn earnings_pool_round_uses_last_update_when_stale() {
        assert_eq!(
            earnings_pool_round(4300.into(), 4371.into()),
            U256::from(4300)
        );
        assert_eq!(
            earnings_pool_round(4371.into(), 4371.into()),
            U256::from(4371)
        );
        // Stake changed this round: lastActiveStakeUpdateRound is already current + 1.
        assert_eq!(
            earnings_pool_round(4372.into(), 4371.into()),
            U256::from(4371)
        );
    }

    // ---------------------------------------------------------------
    // Insert position
    // ---------------------------------------------------------------

    #[test]
    fn hints_stay_in_place_when_reward_does_not_pass_anyone() {
        let p = pool(&[500, 400, 300, 200]);
        // addr(3) goes 300 -> 310: still between 400 and 200.
        assert_eq!(hints(&p, addr(3), 310.into()), (addr(2), addr(4)));
    }

    #[test]
    fn hints_move_up_when_reward_passes_a_neighbour() {
        let p = pool(&[500, 400, 300, 200]);
        assert_eq!(hints(&p, addr(3), 450.into()), (addr(1), addr(2)));
    }

    #[test]
    fn hints_for_new_head() {
        let p = pool(&[500, 400, 300]);
        assert_eq!(hints(&p, addr(3), 600.into()), (Address::zero(), addr(1)));
        // Already the head and staying there.
        assert_eq!(hints(&p, addr(1), 510.into()), (Address::zero(), addr(2)));
    }

    #[test]
    fn hints_for_tail() {
        let p = pool(&[500, 400, 300]);
        assert_eq!(hints(&p, addr(3), 301.into()), (addr(2), Address::zero()));
    }

    #[test]
    fn hints_for_only_member() {
        let p = pool(&[500]);
        assert_eq!(
            hints(&p, addr(1), 600.into()),
            (Address::zero(), Address::zero())
        );
    }

    #[test]
    fn hints_with_equal_stake_go_after_the_tie() {
        let p = pool(&[500, 400, 400, 100]);
        let (prev, next) = hints(&p, addr(4), 400.into());
        assert_eq!((prev, next), (addr(3), Address::zero()));
        assert!(valid_insert_position(&p, addr(4), 400.into(), prev, next));
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
        block_on(find_insert_hints(&counting, addr(2), 410.into(), 100)).unwrap();
        // Reads 500 then 300 (skipping itself) and stops.
        assert_eq!(counting.1.get(), 2);
    }

    #[test]
    fn walk_errors_past_max_nodes() {
        let p = VecPool::new(&pool(&[500, 400, 300]));
        let err = block_on(find_insert_hints(&p, addr(9), 1.into(), 2)).unwrap_err();
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

/// `compute_reward_hints` against a scripted JSON-RPC provider.
#[cfg(test)]
mod rpc_tests {
    use std::sync::Arc;

    use ethers::{
        abi::{Token, encode},
        contract::ContractCall,
        providers::{JsonRpcError, MockProvider, MockResponse, Provider},
        types::{BlockNumber, Bytes},
        utils::serialize,
    };

    use super::*;

    type Client = Provider<MockProvider>;

    const BONDING: u64 = 0xb0;
    const CONTROLLER: u64 = 0xc0;
    const MINTER: u64 = 0xd0;
    const ME: u64 = 3;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn setup() -> (BondingManager<Client>, Arc<Client>, MockProvider) {
        let (provider, mock) = Provider::mocked();
        let client = Arc::new(provider);
        (
            BondingManager::new(addr(BONDING), client.clone()),
            client,
            mock,
        )
    }

    /// MockProvider pops responses from the back, so push them in reverse call order.
    fn script(mock: &MockProvider, responses: Vec<MockResponse>) {
        for r in responses.into_iter().rev() {
            mock.push_response(r);
        }
    }

    /// ABI-encoded eth_call return data.
    fn ret(tokens: &[Token]) -> MockResponse {
        MockResponse::Value(serialize(&Bytes::from(encode(tokens))))
    }

    fn uint(n: u64) -> Token {
        Token::Uint(n.into())
    }

    fn address(a: Address) -> Token {
        Token::Address(a)
    }

    fn rpc_error() -> MockResponse {
        MockResponse::Error(JsonRpcError {
            code: -32000,
            message: "boom".into(),
            data: None,
        })
    }

    /// getTranscoder() return values with the given lastActiveStakeUpdateRound.
    fn transcoder(last_active_stake_update_round: u64) -> MockResponse {
        let mut t = vec![uint(0); 10];
        t[0] = uint(4370);
        t[3] = uint(last_active_stake_update_round);
        ret(&t)
    }

    fn earnings_pool(total_stake: u64) -> MockResponse {
        ret(&[uint(total_stake), uint(0), uint(0), uint(0), uint(0)])
    }

    /// Asserts the next request is an eth_call of `call` at the latest block.
    fn expect_call<D>(mock: &MockProvider, call: &ContractCall<Client, D>) {
        mock.assert_request(
            "eth_call",
            [serialize(&call.tx), serialize(&BlockNumber::Latest)],
        )
        .unwrap();
    }

    /// Responses up to and including getTranscoderPoolMaxSize (before the pool walk).
    fn inputs(last_active_round: u64, max_pool: u64) -> Vec<MockResponse> {
        vec![
            ret(&[address(addr(CONTROLLER))]),
            ret(&[address(addr(MINTER))]),
            transcoder(last_active_round),
            // EarningsPool.totalStake 250 of 1000 active, 1000 mintable, 10% treasury cut:
            // reward = 250 - 25 = 225.
            earnings_pool(250),
            ret(&[uint(1000)]),
            ret(&[uint(1000)]),
            ret(&[Token::Uint(U256::exp10(26))]),
            ret(&[uint(300)]),
            ret(&[uint(max_pool)]),
        ]
    }

    #[tokio::test]
    async fn computes_hints_from_chain_state() {
        let (bonding, client, mock) = setup();
        // Pool: 1=500, 2=400, me=300, 4=200. 300 + 225 = 525 passes everyone.
        let mut responses = inputs(4300, 100);
        responses.extend([ret(&[address(addr(1))]), ret(&[uint(500)])]);
        script(&mock, responses);

        let hints = compute_reward_hints(&bonding, addr(ME), 4371.into())
            .await
            .unwrap();
        assert_eq!(
            hints,
            RewardHints {
                prev: Address::zero(),
                next: addr(1),
                current_stake: 300.into(),
                expected_reward: 225.into(),
            }
        );

        // The Minter is resolved through the Controller the BondingManager reports, and the
        // earnings pool is read for lastActiveStakeUpdateRound (stale stake).
        let controller = Controller::new(addr(CONTROLLER), client.clone());
        let minter = Minter::new(addr(MINTER), client);
        expect_call(&mock, &bonding.controller());
        expect_call(&mock, &controller.get_contract(keccak256("Minter")));
        expect_call(&mock, &bonding.get_transcoder(addr(ME)));
        expect_call(
            &mock,
            &bonding.get_transcoder_earnings_pool_for_round(addr(ME), 4300.into()),
        );
        expect_call(&mock, &minter.current_mintable_tokens());
        expect_call(&mock, &bonding.current_round_total_active_stake());
        expect_call(&mock, &bonding.treasury_reward_cut_rate());
        expect_call(&mock, &bonding.transcoder_total_stake(addr(ME)));
        expect_call(&mock, &bonding.get_transcoder_pool_max_size());
        expect_call(&mock, &bonding.get_first_transcoder_in_pool());
        expect_call(&mock, &bonding.transcoder_total_stake(addr(1)));
        assert!(
            mock.assert_request("eth_call", ()).is_err(),
            "no more requests"
        );
    }

    #[tokio::test]
    async fn uses_current_round_pool_when_stake_updated_this_round() {
        let (bonding, client, mock) = setup();
        // lastActiveStakeUpdateRound = current + 1 -> pool for the current round.
        let mut responses = inputs(4372, 100);
        // Walk: 1=600 stays ahead, me skipped, 4=200 is below 525.
        responses.extend([
            ret(&[address(addr(1))]),
            ret(&[uint(600)]),
            ret(&[address(addr(ME))]),
            ret(&[address(addr(4))]),
            ret(&[uint(200)]),
        ]);
        script(&mock, responses);

        let hints = compute_reward_hints(&bonding, addr(ME), 4371.into())
            .await
            .unwrap();
        assert_eq!((hints.prev, hints.next), (addr(1), addr(4)));

        let controller = Controller::new(addr(CONTROLLER), client);
        expect_call(&mock, &bonding.controller());
        expect_call(&mock, &controller.get_contract(keccak256("Minter")));
        expect_call(&mock, &bonding.get_transcoder(addr(ME)));
        expect_call(
            &mock,
            &bonding.get_transcoder_earnings_pool_for_round(addr(ME), 4371.into()),
        );
    }

    #[tokio::test]
    async fn empty_pool_gives_zero_hints() {
        let (bonding, _client, mock) = setup();
        let mut responses = inputs(4371, 100);
        responses.push(ret(&[address(Address::zero())]));
        script(&mock, responses);

        let hints = compute_reward_hints(&bonding, addr(ME), 4371.into())
            .await
            .unwrap();
        assert_eq!((hints.prev, hints.next), (Address::zero(), Address::zero()));
        assert_eq!(hints.expected_reward, U256::from(225));
    }

    #[tokio::test]
    async fn pool_max_size_bounds_the_walk() {
        let (bonding, _client, mock) = setup();
        let mut responses = inputs(4371, 0);
        responses.push(ret(&[address(addr(1))]));
        script(&mock, responses);

        let err = compute_reward_hints(&bonding, addr(ME), 4371.into())
            .await
            .unwrap_err();
        assert!(matches!(err, HintError::PoolTooLong(0)), "{err}");
        assert_eq!(err.to_string(), "pool walk exceeded 0 nodes");
    }

    #[tokio::test]
    async fn rpc_errors_are_returned_at_every_step() {
        // Fail each of the nine input reads, plus the first pool read.
        for fail_at in 0..10 {
            let (bonding, _client, mock) = setup();
            let mut responses = inputs(4371, 100);
            responses.push(ret(&[address(addr(1))]));
            responses.truncate(fail_at);
            responses.push(rpc_error());
            script(&mock, responses);

            let err = compute_reward_hints(&bonding, addr(ME), 4371.into())
                .await
                .unwrap_err();
            assert!(matches!(err, HintError::Source(_)), "step {fail_at}: {err}");
            assert!(err.to_string().contains("boom"), "step {fail_at}: {err}");
        }
    }

    #[tokio::test]
    async fn malformed_return_data_is_an_error() {
        let (bonding, _client, mock) = setup();
        // controller() returns no data.
        script(&mock, vec![ret(&[])]);
        let err = compute_reward_hints(&bonding, addr(ME), 4371.into())
            .await
            .unwrap_err();
        assert!(matches!(err, HintError::Source(_)));
    }
}
