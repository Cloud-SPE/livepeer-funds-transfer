//! Tests for the main-loop actions against a scripted JSON-RPC provider.
//!
//! `MockProvider` pops responses from the back of its queue and records requests in order, so
//! [`script`] pushes responses in reverse call order and [`expect_call`] and friends assert
//! requests front to back.

use std::{sync::Arc, time::Duration};

use ethers::{
    abi::{Token, encode},
    providers::{JsonRpcError, MockProvider, MockResponse},
    types::{Block, Bytes, Transaction, TransactionReceipt, U64},
    utils::{keccak256, serialize},
};
use proptest::prelude::*;
use serde::Serialize;

use livepeer_funds_transfer::contracts::{Controller, Minter};

use super::*;

type Client = Provider<MockProvider>;

const ROUNDS: u64 = 0xa0;
const BONDING: u64 = 0xb0;
const ORCH: u64 = 0x01;
const LPT_RECEIVER: u64 = 0x02;
const ETH_RECEIVER: u64 = 0x03;
const HEAD: u64 = 0x11;
const GAS: u64 = 500_000;
const BASE_FEE: u64 = 10_000_000;

fn addr(n: u64) -> Address {
    Address::from_low_u64_be(n)
}

fn tx_hash() -> TxHash {
    TxHash::from(keccak256("tx"))
}

struct Harness {
    client: Arc<Client>,
    mock: MockProvider,
    rounds: RoundsManager<Client>,
    bonding: BondingManager<Client>,
}

fn harness() -> Harness {
    let (provider, mock) = Provider::mocked();
    // Short poll interval so PendingTransaction resolves quickly.
    let client = Arc::new(provider.interval(Duration::from_millis(1)));
    Harness {
        rounds: RoundsManager::new(addr(ROUNDS), client.clone()),
        bonding: BondingManager::new(addr(BONDING), client.clone()),
        client,
        mock,
    }
}

fn cfg() -> Config {
    Config {
        http_rpc_url: "http://localhost:8545".into(),
        chain_id: 42161,
        rounds_manager_addr: addr(ROUNDS),
        bonding_manager_addr: addr(BONDING),
        json_key_file: "key.json".into(),
        passphrase_file: "pass.txt".into(),
        orchestrator_addr: None,
        loop_sleep_secs: 6,
        receipt_timeout_secs: 90,
        max_fee_base_multiplier: 2,
        max_priority_fee_wei: U256::zero(),
        enable_reward: true,
        enable_transfer_bond: true,
        lpt_receiver_addr: Some(addr(LPT_RECEIVER)),
        lpt_min_retain_wei: Some(1000.into()),
        enable_withdraw_fees: true,
        eth_fee_receiver_addr: Some(addr(ETH_RECEIVER)),
        eth_fee_withdraw_threshold_wei: Some(2000.into()),
    }
}

// -------------------------------------------------------------------
// Responses
// -------------------------------------------------------------------

/// Pushes `responses` so they are returned in the given order.
fn script(mock: &MockProvider, responses: Vec<MockResponse>) {
    for r in responses.into_iter().rev() {
        mock.push_response(r);
    }
}

fn value<T: Serialize>(v: &T) -> MockResponse {
    MockResponse::Value(serialize(v))
}

/// ABI-encoded eth_call return data.
fn ret(tokens: &[Token]) -> MockResponse {
    value(&Bytes::from(encode(tokens)))
}

fn uint(n: u64) -> MockResponse {
    ret(&[Token::Uint(n.into())])
}

fn boolean(b: bool) -> MockResponse {
    ret(&[Token::Bool(b)])
}

fn rpc_error() -> MockResponse {
    MockResponse::Error(JsonRpcError {
        code: -32000,
        message: "boom".into(),
        data: None,
    })
}

fn round_state(round: u64, initialized: bool, locked: bool) -> Vec<MockResponse> {
    vec![uint(round), boolean(initialized), boolean(locked)]
}

/// getTranscoder() with the given lastRewardRound.
fn transcoder(last_reward_round: u64) -> MockResponse {
    let mut t = vec![Token::Uint(U256::zero()); 10];
    t[0] = Token::Uint(last_reward_round.into());
    ret(&t)
}

fn latest_block(base_fee: Option<u64>) -> MockResponse {
    value(&Some(Block::<TxHash> {
        number: Some(U64::from(100)),
        base_fee_per_gas: base_fee.map(U256::from),
        ..Default::default()
    }))
}

/// Everything after the fee lookup for a tx that gets mined: estimateGas, send, then the
/// PendingTransaction polls (tx, receipt).
fn mined() -> Vec<MockResponse> {
    vec![
        uint_value(GAS),
        value(&tx_hash()),
        value(&Some(Transaction {
            hash: tx_hash(),
            block_number: Some(U64::from(101)),
            ..Default::default()
        })),
        value(&Some(TransactionReceipt {
            transaction_hash: tx_hash(),
            block_number: Some(U64::from(101)),
            status: Some(U64::from(1)),
            gas_used: Some(U256::from(GAS / 2)),
            ..Default::default()
        })),
    ]
}

/// A tx that is sent but never found: PendingTransaction gives up after 3 retries.
fn dropped() -> Vec<MockResponse> {
    let mut r = vec![uint_value(GAS), value(&tx_hash())];
    r.extend((0..4).map(|_| value(&Option::<Transaction>::None)));
    r
}

/// A tx that is sent; with a zero receipt timeout nothing is polled.
fn sent() -> Vec<MockResponse> {
    vec![uint_value(GAS), value(&tx_hash())]
}

/// A JSON number-as-hex response (eth_estimateGas).
fn uint_value(n: u64) -> MockResponse {
    value(&U256::from(n))
}

// -------------------------------------------------------------------
// Request assertions
// -------------------------------------------------------------------

/// Asserts the next request is an eth_call of `call` at the latest block.
fn expect_call<D>(mock: &MockProvider, call: &ContractCall<Client, D>) {
    expect_eth_call(mock, &call.tx);
}

fn expect_eth_call(mock: &MockProvider, tx: &TypedTransaction) {
    mock.assert_request("eth_call", [serialize(tx), serialize(&BlockNumber::Latest)])
        .unwrap();
}

fn expect_latest_block(mock: &MockProvider) {
    mock.assert_request(
        "eth_getBlockByNumber",
        [serialize(&BlockNumber::Latest), serialize(&false)],
    )
    .unwrap();
}

/// Asserts estimateGas then sendTransaction for `call` with the fees `with_l2_fees` sets.
fn expect_send<D>(mock: &MockProvider, call: &ContractCall<Client, D>, max_fee: u64, tip: u64) {
    let mut tx = call.tx.clone();
    if let TypedTransaction::Eip1559(inner) = &mut tx {
        inner.max_fee_per_gas = Some(max_fee.into());
        inner.max_priority_fee_per_gas = Some(tip.into());
    } else {
        panic!("expected an EIP-1559 tx");
    }
    mock.assert_request("eth_estimateGas", [serialize(&tx)])
        .unwrap();
    tx.set_gas(GAS);
    mock.assert_request("eth_sendTransaction", [serialize(&tx)])
        .unwrap();
}

fn expect_no_more_requests(mock: &MockProvider) {
    // assert_request only returns Err when the request queue is empty.
    assert!(mock.assert_request("none", ()).is_err());
}

fn expect_round_state(h: &Harness) {
    expect_call(&h.mock, &h.rounds.current_round());
    expect_call(&h.mock, &h.rounds.current_round_initialized());
    expect_call(&h.mock, &h.rounds.current_round_locked());
}

// -------------------------------------------------------------------
// fetch_round_state
// -------------------------------------------------------------------

#[tokio::test]
async fn fetch_round_state_reads_all_three_flags() {
    let h = harness();
    script(&h.mock, round_state(4371, true, false));
    let state = fetch_round_state(&h.rounds).await.unwrap();
    assert_eq!(
        state,
        RoundState {
            round: 4371.into(),
            initialized: true,
            locked: false,
        }
    );
    expect_round_state(&h);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn fetch_round_state_reports_which_call_failed() {
    let calls = [
        "RoundsManager.currentRound() failed",
        "RoundsManager.currentRoundInitialized() failed",
        "RoundsManager.currentRoundLocked() failed",
    ];
    for (fail_at, expected) in calls.iter().enumerate() {
        let h = harness();
        let mut responses = round_state(4371, true, true);
        responses.truncate(fail_at);
        responses.push(rpc_error());
        script(&h.mock, responses);

        let err = fetch_round_state(&h.rounds).await.unwrap_err();
        assert!(matches!(err, AppError::Contract(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.starts_with(&format!("contract error: {expected}")),
            "{msg}"
        );
        assert!(msg.contains("boom"), "{msg}");
    }
}

// -------------------------------------------------------------------
// with_l2_fees
// -------------------------------------------------------------------

fn fees(call: &ContractCall<Client, ()>) -> (Option<U256>, Option<U256>) {
    match &call.tx {
        TypedTransaction::Eip1559(inner) => (inner.max_fee_per_gas, inner.max_priority_fee_per_gas),
        other => panic!("expected EIP-1559, got {other:?}"),
    }
}

#[tokio::test]
async fn with_l2_fees_uses_base_fee_times_multiplier_plus_tip() {
    let h = harness();
    script(&h.mock, vec![latest_block(Some(BASE_FEE))]);
    let mut cfg = cfg();
    cfg.max_fee_base_multiplier = 3;
    cfg.max_priority_fee_wei = 7.into();

    let call = h.bonding.reward_with_hint(addr(1), addr(2));
    let call = with_l2_fees(h.client.as_ref(), call, &cfg).await.unwrap();
    assert_eq!(
        fees(&call),
        (Some(U256::from(3 * BASE_FEE + 7)), Some(U256::from(7)))
    );
    // Calldata and target are untouched.
    assert_eq!(
        call.tx.data(),
        h.bonding.reward_with_hint(addr(1), addr(2)).tx.data()
    );
    assert_eq!(call.tx.to(), Some(&addr(BONDING).into()));
    expect_latest_block(&h.mock);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn with_l2_fees_errors_without_latest_block() {
    let h = harness();
    script(&h.mock, vec![value(&Option::<Block<TxHash>>::None)]);
    let err = with_l2_fees(h.client.as_ref(), h.bonding.reward(), &cfg())
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Provider(_)));
    assert_eq!(err.to_string(), "provider error: latest block not found");
}

#[tokio::test]
async fn with_l2_fees_errors_without_base_fee() {
    let h = harness();
    script(&h.mock, vec![latest_block(None)]);
    let err = with_l2_fees(h.client.as_ref(), h.bonding.reward(), &cfg())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "provider error: latest block has no baseFeePerGas"
    );
}

#[tokio::test]
async fn with_l2_fees_reports_rpc_errors() {
    let h = harness();
    script(&h.mock, vec![rpc_error()]);
    let err = with_l2_fees(h.client.as_ref(), h.bonding.reward(), &cfg())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("provider error: get_block(latest) failed:"),
        "{msg}"
    );
    assert!(msg.contains("boom"), "{msg}");
}

#[tokio::test]
async fn with_l2_fees_leaves_legacy_tx_untouched() {
    let h = harness();
    script(&h.mock, vec![latest_block(Some(BASE_FEE))]);
    let call = h.bonding.reward().legacy();
    let before = call.tx.clone();
    let call = with_l2_fees(h.client.as_ref(), call, &cfg()).await.unwrap();
    assert_eq!(call.tx, before);
    assert_eq!(call.tx.gas_price(), None);
}

proptest! {
    #[test]
    fn with_l2_fees_math(
        base_fee in any::<u64>(),
        multiplier in 1u64..=u64::MAX,
        tip in any::<u64>(),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = harness();
        script(&h.mock, vec![latest_block(Some(base_fee))]);
        let mut cfg = cfg();
        cfg.max_fee_base_multiplier = multiplier;
        cfg.max_priority_fee_wei = tip.into();

        let call = rt
            .block_on(with_l2_fees(h.client.as_ref(), h.bonding.reward(), &cfg))
            .unwrap();
        let expected = U256::from(base_fee) * U256::from(multiplier) + U256::from(tip);
        prop_assert_eq!(fees(&call), (Some(expected), Some(U256::from(tip))));
    }
}

// -------------------------------------------------------------------
// maybe_reward_once_per_round
// -------------------------------------------------------------------

#[tokio::test]
async fn reward_skipped_when_already_rewarded_this_round() {
    for last_reward_round in [4371, 4372] {
        let h = harness();
        script(&h.mock, vec![transcoder(last_reward_round)]);
        maybe_reward_once_per_round(&h.bonding, addr(ORCH), 4371.into(), &cfg())
            .await
            .unwrap();
        expect_call(&h.mock, &h.bonding.get_transcoder(addr(ORCH)));
        expect_no_more_requests(&h.mock);
    }
}

#[tokio::test]
async fn reward_reports_get_transcoder_failure() {
    let h = harness();
    script(&h.mock, vec![rpc_error()]);
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), 4371.into(), &cfg())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("contract error: BondingManager.getTranscoder() failed:"),
        "{msg}"
    );
}

/// Responses for compute_reward_hints on a pool `[HEAD=500, ORCH=300]`, where ORCH's reward
/// of 225 moves it to the head. ORCH's stake was last updated in round 4300.
fn hint_responses() -> Vec<MockResponse> {
    let address = |a: Address| ret(&[Token::Address(a)]);
    vec![
        address(addr(0xc0)),
        address(addr(0xd0)),
        ret(&[
            Token::Uint(4370.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
            Token::Uint(4300.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
        ]),
        ret(&[
            Token::Uint(250.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
            Token::Uint(0.into()),
        ]),
        uint(1000),
        uint(1000),
        ret(&[Token::Uint(U256::exp10(26))]),
        uint(300),
        uint(100),
        address(addr(HEAD)),
        uint(500),
    ]
}

#[tokio::test]
async fn reward_sends_reward_with_hint_using_computed_hints() {
    let h = harness();
    script(
        &h.mock,
        [
            vec![transcoder(4370)],
            hint_responses(),
            vec![latest_block(Some(BASE_FEE))],
            mined(),
        ]
        .concat(),
    );

    maybe_reward_once_per_round(&h.bonding, addr(ORCH), 4371.into(), &cfg())
        .await
        .unwrap();

    expect_call(&h.mock, &h.bonding.get_transcoder(addr(ORCH)));
    expect_hint_calls(&h);
    expect_latest_block(&h.mock);
    expect_send(
        &h.mock,
        &h.bonding.reward_with_hint(Address::zero(), addr(HEAD)),
        2 * BASE_FEE,
        0,
    );
    expect_mined(&h.mock);
    expect_no_more_requests(&h.mock);
}

/// The eth_calls compute_reward_hints makes for [`hint_responses`].
fn expect_hint_calls(h: &Harness) {
    let controller = Controller::new(addr(0xc0), h.client.clone());
    let minter = Minter::new(addr(0xd0), h.client.clone());
    let b = &h.bonding;
    expect_call(&h.mock, &b.controller());
    expect_call(&h.mock, &controller.get_contract(keccak256("Minter")));
    expect_call(&h.mock, &b.get_transcoder(addr(ORCH)));
    expect_call(
        &h.mock,
        &b.get_transcoder_earnings_pool_for_round(addr(ORCH), 4300.into()),
    );
    expect_call(&h.mock, &minter.current_mintable_tokens());
    expect_call(&h.mock, &b.current_round_total_active_stake());
    expect_call(&h.mock, &b.treasury_reward_cut_rate());
    expect_call(&h.mock, &b.transcoder_total_stake(addr(ORCH)));
    expect_call(&h.mock, &b.get_transcoder_pool_max_size());
    expect_call(&h.mock, &b.get_first_transcoder_in_pool());
    expect_call(&h.mock, &b.transcoder_total_stake(addr(HEAD)));
}

/// The PendingTransaction polls for [`mined`].
fn expect_mined(mock: &MockProvider) {
    mock.assert_request("eth_getTransactionByHash", [tx_hash()])
        .unwrap();
    mock.assert_request("eth_getTransactionReceipt", [tx_hash()])
        .unwrap();
}

/// Responses for a reward that is needed, with hints failing at the first read, up to and
/// including the fee lookup.
fn reward_needed() -> Vec<MockResponse> {
    vec![transcoder(4370), rpc_error(), latest_block(Some(BASE_FEE))]
}

#[tokio::test]
async fn reward_falls_back_to_zero_hints_when_hints_fail() {
    let h = harness();
    script(&h.mock, [reward_needed(), mined()].concat());
    let mut cfg = cfg();
    cfg.max_priority_fee_wei = 5.into();

    maybe_reward_once_per_round(&h.bonding, addr(ORCH), 4371.into(), &cfg)
        .await
        .unwrap();

    expect_call(&h.mock, &h.bonding.get_transcoder(addr(ORCH)));
    expect_call(&h.mock, &h.bonding.controller());
    expect_latest_block(&h.mock);
    expect_send(
        &h.mock,
        &h.bonding.reward_with_hint(Address::zero(), Address::zero()),
        2 * BASE_FEE + 5,
        5,
    );
    expect_mined(&h.mock);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn reward_fee_lookup_failure_is_returned() {
    let h = harness();
    script(
        &h.mock,
        vec![transcoder(4370), rpc_error(), latest_block(None)],
    );
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), 4371.into(), &cfg())
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Provider(_)), "{err}");
}

#[tokio::test]
async fn reward_send_failure_is_returned() {
    let h = harness();
    // eth_estimateGas fails inside send().
    script(&h.mock, [reward_needed(), vec![rpc_error()]].concat());
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), 4371.into(), &cfg())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("tx error: rewardWithHint() send failed:"),
        "{msg}"
    );
}

#[tokio::test]
async fn reward_dropped_tx_is_an_error() {
    let h = harness();
    script(&h.mock, [reward_needed(), dropped()].concat());
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), 4371.into(), &cfg())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "tx error: reward tx pending returned None receipt: tx_hash={:?}",
            tx_hash()
        )
    );
}

#[tokio::test]
async fn reward_receipt_timeout_is_an_error() {
    let h = harness();
    script(&h.mock, [reward_needed(), sent()].concat());
    let mut cfg = cfg();
    cfg.receipt_timeout_secs = 0;
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), 4371.into(), &cfg)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "tx error: reward tx receipt timeout after 0s: tx_hash={:?}",
            tx_hash()
        )
    );
}

#[tokio::test]
async fn reward_through_signer_middleware_sends_signed_tx() {
    // Same client stack as main(): SignerMiddleware over a provider, with a local key.
    let (provider, mock) = Provider::mocked();
    let wallet = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
        .parse::<LocalWallet>()
        .unwrap()
        .with_chain_id(42161u64);
    let signer = wallet.address();
    let client = Arc::new(SignerMiddleware::new(
        provider.interval(Duration::from_millis(1)),
        wallet.clone(),
    ));
    let bonding = BondingManager::new(addr(BONDING), client);

    // getTranscoder, controller() fails, block, nonce, estimateGas, sendRawTransaction,
    // then the tx/receipt polls.
    script(
        &mock,
        [reward_needed(), vec![uint_value(7)], mined()].concat(),
    );

    maybe_reward_once_per_round(&bonding, signer, 4371.into(), &cfg())
        .await
        .unwrap();

    // SignerMiddleware sets `from` on calls.
    let with_from = |mut tx: TypedTransaction| {
        tx.set_from(signer);
        tx
    };
    expect_eth_call(&mock, &with_from(bonding.get_transcoder(signer).tx));
    expect_eth_call(&mock, &with_from(bonding.controller().tx));
    expect_latest_block(&mock);
    mock.assert_request(
        "eth_getTransactionCount",
        [serialize(&signer), serialize(&BlockNumber::Latest)],
    )
    .unwrap();

    let mut tx = with_from(
        bonding
            .reward_with_hint(Address::zero(), Address::zero())
            .tx,
    );
    tx.set_chain_id(42161u64);
    tx.set_nonce(7);
    if let TypedTransaction::Eip1559(inner) = &mut tx {
        inner.max_fee_per_gas = Some((2 * BASE_FEE).into());
        inner.max_priority_fee_per_gas = Some(U256::zero());
    }
    mock.assert_request("eth_estimateGas", [serialize(&tx)])
        .unwrap();
    tx.set_gas(GAS);
    let sig = wallet.sign_transaction_sync(&tx).unwrap();
    mock.assert_request("eth_sendRawTransaction", [tx.rlp_signed(&sig)])
        .unwrap();
    expect_mined(&mock);
    expect_no_more_requests(&mock);
}

// -------------------------------------------------------------------
// handle_locked_round_actions
// -------------------------------------------------------------------

async fn locked(
    h: &Harness,
    cfg: &Config,
    snapshot: &mut Option<LockedSnapshot>,
) -> Result<(), AppError> {
    handle_locked_round_actions(&h.bonding, addr(ORCH), 4371.into(), cfg, snapshot).await
}

fn snapshot(stake: Option<u64>, fees: Option<u64>) -> Option<LockedSnapshot> {
    Some(LockedSnapshot {
        round: 4371.into(),
        pending_stake: stake.unwrap_or(0).into(),
        pending_fees: fees.unwrap_or(0).into(),
        stake_present: stake.is_some(),
        fees_present: fees.is_some(),
    })
}

fn only_transfer_bond() -> Config {
    Config {
        enable_withdraw_fees: false,
        ..cfg()
    }
}

fn only_withdraw_fees() -> Config {
    Config {
        enable_transfer_bond: false,
        ..cfg()
    }
}

#[tokio::test]
async fn locked_actions_do_nothing_when_both_disabled() {
    let h = harness();
    let cfg = Config {
        enable_transfer_bond: false,
        enable_withdraw_fees: false,
        ..cfg()
    };
    let mut snap = None;
    locked(&h, &cfg, &mut snap).await.unwrap();
    assert_eq!(snap, None);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn locked_actions_require_settings_for_enabled_actions() {
    let cases = [
        (
            Config {
                lpt_receiver_addr: None,
                ..cfg()
            },
            "LPT_RECEIVER_ADDR",
        ),
        (
            Config {
                lpt_min_retain_wei: None,
                ..cfg()
            },
            "LPT_MIN_RETAIN_WEI",
        ),
        (
            Config {
                eth_fee_receiver_addr: None,
                ..only_withdraw_fees()
            },
            "ETH_FEE_RECEIVER_ADDR",
        ),
        (
            Config {
                eth_fee_withdraw_threshold_wei: None,
                ..only_withdraw_fees()
            },
            "ETH_FEE_WITHDRAW_THRESHOLD_WEI",
        ),
    ];
    for (cfg, key) in cases {
        let h = harness();
        let err = locked(&h, &cfg, &mut None).await.unwrap_err();
        assert!(matches!(err, AppError::BadEnv(k, _) if k == key), "{err}");
        expect_no_more_requests(&h.mock);
    }
}

#[tokio::test]
async fn transfer_bond_skipped_when_nothing_above_retain() {
    // Below, equal to, and zero pending stake with retain = 1000.
    for pending in [999, 1000, 0] {
        let h = harness();
        script(&h.mock, vec![uint(pending)]);
        let mut snap = None;
        locked(&h, &only_transfer_bond(), &mut snap).await.unwrap();
        assert_eq!(snap, snapshot(Some(pending), None));
        expect_call(&h.mock, &h.bonding.pending_stake(addr(ORCH), 4371.into()));
        expect_no_more_requests(&h.mock);
    }
}

#[tokio::test]
async fn transfer_bond_sends_stake_above_retain() {
    let h = harness();
    script(
        &h.mock,
        [vec![uint(1500), latest_block(Some(BASE_FEE))], mined()].concat(),
    );
    let mut snap = None;
    locked(&h, &only_transfer_bond(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(Some(1500), None));

    expect_call(&h.mock, &h.bonding.pending_stake(addr(ORCH), 4371.into()));
    expect_latest_block(&h.mock);
    let zero = Address::zero();
    expect_send(
        &h.mock,
        &h.bonding
            .transfer_bond(addr(LPT_RECEIVER), 500.into(), zero, zero, zero, zero),
        2 * BASE_FEE,
        0,
    );
    expect_mined(&h.mock);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn transfer_bond_with_zero_retain_sends_everything() {
    let h = harness();
    script(
        &h.mock,
        [vec![uint(1500), latest_block(Some(BASE_FEE))], mined()].concat(),
    );
    let cfg = Config {
        lpt_min_retain_wei: Some(U256::zero()),
        ..only_transfer_bond()
    };
    locked(&h, &cfg, &mut None).await.unwrap();

    expect_call(&h.mock, &h.bonding.pending_stake(addr(ORCH), 4371.into()));
    expect_latest_block(&h.mock);
    let zero = Address::zero();
    expect_send(
        &h.mock,
        &h.bonding
            .transfer_bond(addr(LPT_RECEIVER), 1500.into(), zero, zero, zero, zero),
        2 * BASE_FEE,
        0,
    );
}

#[tokio::test]
async fn transfer_bond_tx_problems_are_logged_not_returned() {
    // Send failure, dropped tx, receipt timeout: all end in Ok with the snapshot updated.
    let cases: [(Vec<MockResponse>, u64); 3] =
        [(vec![rpc_error()], 90), (dropped(), 90), (sent(), 0)];
    for (tx_responses, receipt_timeout_secs) in cases {
        let h = harness();
        script(
            &h.mock,
            [vec![uint(1500), latest_block(Some(BASE_FEE))], tx_responses].concat(),
        );
        let cfg = Config {
            receipt_timeout_secs,
            ..only_transfer_bond()
        };
        let mut snap = None;
        locked(&h, &cfg, &mut snap).await.unwrap();
        assert_eq!(snap, snapshot(Some(1500), None));
    }
}

#[tokio::test]
async fn transfer_bond_send_failure_still_withdraws_fees() {
    let h = harness();
    script(
        &h.mock,
        [
            vec![uint(1500), latest_block(Some(BASE_FEE)), rpc_error()],
            vec![uint(3000), latest_block(Some(BASE_FEE))],
            mined(),
        ]
        .concat(),
    );
    let mut snap = None;
    locked(&h, &cfg(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(Some(1500), Some(3000)));

    expect_call(&h.mock, &h.bonding.pending_stake(addr(ORCH), 4371.into()));
    expect_latest_block(&h.mock);
    let mut failed = h.bonding.transfer_bond(
        addr(LPT_RECEIVER),
        500.into(),
        Address::zero(),
        Address::zero(),
        Address::zero(),
        Address::zero(),
    );
    if let TypedTransaction::Eip1559(inner) = &mut failed.tx {
        inner.max_fee_per_gas = Some((2 * BASE_FEE).into());
        inner.max_priority_fee_per_gas = Some(U256::zero());
    }
    h.mock
        .assert_request("eth_estimateGas", [serialize(&failed.tx)])
        .unwrap();
    expect_call(&h.mock, &h.bonding.pending_fees(addr(ORCH), 4371.into()));
    expect_latest_block(&h.mock);
    expect_send(
        &h.mock,
        &h.bonding.withdraw_fees(addr(ETH_RECEIVER), 3000.into()),
        2 * BASE_FEE,
        0,
    );
    expect_mined(&h.mock);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn transfer_bond_fee_lookup_failure_aborts_locked_actions() {
    let h = harness();
    script(&h.mock, vec![uint(1500), latest_block(None)]);
    let mut snap = None;
    let err = locked(&h, &cfg(), &mut snap).await.unwrap_err();
    assert!(matches!(err, AppError::Provider(_)), "{err}");
    // withdrawFees is not attempted and the snapshot is not updated.
    assert_eq!(snap, None);
    expect_call(&h.mock, &h.bonding.pending_stake(addr(ORCH), 4371.into()));
    expect_latest_block(&h.mock);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn pending_stake_failure_is_returned() {
    let h = harness();
    script(&h.mock, vec![rpc_error()]);
    let err = locked(&h, &cfg(), &mut None).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("contract error: pendingStake() failed:"),
        "{msg}"
    );
}

#[tokio::test]
async fn pending_fees_failure_is_returned() {
    let h = harness();
    script(&h.mock, vec![uint(0), rpc_error()]);
    let mut snap = None;
    let err = locked(&h, &cfg(), &mut snap).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("contract error: pendingFees() failed:"),
        "{msg}"
    );
    assert_eq!(snap, None);
}

#[tokio::test]
async fn withdraw_fees_skipped_below_threshold() {
    let h = harness();
    script(&h.mock, vec![uint(1999)]);
    let mut snap = None;
    locked(&h, &only_withdraw_fees(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(None, Some(1999)));
    expect_call(&h.mock, &h.bonding.pending_fees(addr(ORCH), 4371.into()));
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn withdraw_fees_skipped_when_zero_even_with_zero_threshold() {
    let h = harness();
    script(&h.mock, vec![uint(0)]);
    let cfg = Config {
        eth_fee_withdraw_threshold_wei: Some(U256::zero()),
        ..only_withdraw_fees()
    };
    locked(&h, &cfg, &mut None).await.unwrap();
    expect_call(&h.mock, &h.bonding.pending_fees(addr(ORCH), 4371.into()));
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn withdraw_fees_sends_all_pending_fees_at_threshold() {
    let h = harness();
    script(
        &h.mock,
        [vec![uint(2000), latest_block(Some(BASE_FEE))], mined()].concat(),
    );
    let mut snap = None;
    locked(&h, &only_withdraw_fees(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(None, Some(2000)));

    expect_call(&h.mock, &h.bonding.pending_fees(addr(ORCH), 4371.into()));
    expect_latest_block(&h.mock);
    expect_send(
        &h.mock,
        &h.bonding.withdraw_fees(addr(ETH_RECEIVER), 2000.into()),
        2 * BASE_FEE,
        0,
    );
    expect_mined(&h.mock);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn withdraw_fees_tx_problems_are_logged_not_returned() {
    let cases: [(Vec<MockResponse>, u64); 3] =
        [(vec![rpc_error()], 90), (dropped(), 90), (sent(), 0)];
    for (tx_responses, receipt_timeout_secs) in cases {
        let h = harness();
        script(
            &h.mock,
            [vec![uint(5000), latest_block(Some(BASE_FEE))], tx_responses].concat(),
        );
        let cfg = Config {
            receipt_timeout_secs,
            ..only_withdraw_fees()
        };
        let mut snap = None;
        locked(&h, &cfg, &mut snap).await.unwrap();
        assert_eq!(snap, snapshot(None, Some(5000)));
    }
}

#[tokio::test]
async fn withdraw_fees_fee_lookup_failure_is_returned() {
    let h = harness();
    script(&h.mock, vec![uint(5000), latest_block(None)]);
    let err = locked(&h, &only_withdraw_fees(), &mut None)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Provider(_)), "{err}");
}

#[tokio::test]
async fn locked_snapshot_tracks_changes() {
    let h = harness();
    // Same values twice, then fees change.
    script(
        &h.mock,
        vec![uint(10), uint(20), uint(10), uint(20), uint(10), uint(30)],
    );
    let mut snap = snapshot(Some(1), None);
    locked(&h, &cfg(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(Some(10), Some(20)));
    locked(&h, &cfg(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(Some(10), Some(20)));
    locked(&h, &cfg(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(Some(10), Some(30)));
}

// -------------------------------------------------------------------
// poll_once
// -------------------------------------------------------------------

async fn poll(
    h: &Harness,
    cfg: &Config,
    last_state: &mut Option<RoundState>,
    snap: &mut Option<LockedSnapshot>,
) {
    poll_once(&h.rounds, &h.bonding, addr(ORCH), cfg, last_state, snap).await;
}

fn state(round: u64, initialized: bool, locked: bool) -> Option<RoundState> {
    Some(RoundState {
        round: round.into(),
        initialized,
        locked,
    })
}

#[tokio::test]
async fn poll_keeps_last_state_when_round_state_fetch_fails() {
    let h = harness();
    script(&h.mock, vec![rpc_error()]);
    let mut last = state(4370, true, true);
    poll(&h, &cfg(), &mut last, &mut None).await;
    assert_eq!(last, state(4370, true, true));
    expect_call(&h.mock, &h.rounds.current_round());
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn poll_checks_reward_when_initialized() {
    let h = harness();
    script(
        &h.mock,
        [round_state(4371, true, false), vec![transcoder(4371)]].concat(),
    );
    let mut last = None;
    let mut snap = None;
    poll(&h, &cfg(), &mut last, &mut snap).await;
    assert_eq!(last, state(4371, true, false));
    assert_eq!(snap, None);
    expect_round_state(&h);
    expect_call(&h.mock, &h.bonding.get_transcoder(addr(ORCH)));
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn poll_skips_reward_when_disabled_or_not_initialized() {
    let cases = [
        (
            Config {
                enable_reward: false,
                ..cfg()
            },
            true,
        ),
        (cfg(), false),
    ];
    for (cfg, initialized) in cases {
        let h = harness();
        script(&h.mock, round_state(4371, initialized, false));
        let mut last = state(4371, initialized, false);
        poll(&h, &cfg, &mut last, &mut None).await;
        assert_eq!(last, state(4371, initialized, false));
        expect_round_state(&h);
        expect_no_more_requests(&h.mock);
    }
}

#[tokio::test]
async fn poll_runs_locked_actions_when_locked() {
    let h = harness();
    // Initialized and locked: reward check first, then both locked-round reads.
    script(
        &h.mock,
        [
            round_state(4371, true, true),
            vec![transcoder(4371), uint(0), uint(0)],
        ]
        .concat(),
    );
    let mut last = state(4371, true, false);
    let mut snap = None;
    poll(&h, &cfg(), &mut last, &mut snap).await;
    assert_eq!(last, state(4371, true, true));
    assert_eq!(snap, snapshot(Some(0), Some(0)));
    expect_round_state(&h);
    expect_call(&h.mock, &h.bonding.get_transcoder(addr(ORCH)));
    expect_call(&h.mock, &h.bonding.pending_stake(addr(ORCH), 4371.into()));
    expect_call(&h.mock, &h.bonding.pending_fees(addr(ORCH), 4371.into()));
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn poll_skips_locked_actions_when_disabled() {
    let h = harness();
    script(&h.mock, round_state(4371, false, true));
    let cfg = Config {
        enable_transfer_bond: false,
        enable_withdraw_fees: false,
        ..cfg()
    };
    let mut snap = None;
    poll(&h, &cfg, &mut None, &mut snap).await;
    assert_eq!(snap, None);
    expect_round_state(&h);
    expect_no_more_requests(&h.mock);
}

#[tokio::test]
async fn poll_logs_action_errors_and_still_records_state() {
    let h = harness();
    // getTranscoder fails, pendingStake fails.
    script(
        &h.mock,
        [
            round_state(4371, true, true),
            vec![rpc_error(), rpc_error()],
        ]
        .concat(),
    );
    let mut last = None;
    let mut snap = None;
    poll(&h, &cfg(), &mut last, &mut snap).await;
    assert_eq!(last, state(4371, true, true));
    assert_eq!(snap, None);
    expect_round_state(&h);
    expect_call(&h.mock, &h.bonding.get_transcoder(addr(ORCH)));
    expect_call(&h.mock, &h.bonding.pending_stake(addr(ORCH), 4371.into()));
    expect_no_more_requests(&h.mock);
}
