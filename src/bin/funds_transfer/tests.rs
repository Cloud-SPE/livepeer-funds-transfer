//! Tests for the main-loop actions against a scripted JSON-RPC transport.
//!
//! Every test runs on the production client stack ([`signing_provider`]: gas estimation, pending
//! nonce, fixed chain id, local signing), so sends are checked as decoded signed transactions.
//! [`MockRpc`] queues responses per RPC method and records requests in order; the `expect_*`
//! helpers assert requests front to back.

use alloy::{
    consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable},
    primitives::{Bytes, TxKind, keccak256},
    providers::DynProvider,
    rpc::types::TransactionRequest,
    sol_types::SolValue,
};
use proptest::prelude::*;

use livepeer_funds_transfer::contracts::{Controller, Minter};

use super::*;
use crate::mock_rpc::MockRpc;

const ROUNDS: u64 = 0xa0;
const BONDING: u64 = 0xb0;
const ORCH: u64 = 0x01;
const LPT_RECEIVER: u64 = 0x02;
const ETH_RECEIVER: u64 = 0x03;
const HEAD: u64 = 0x11;
const GAS: u64 = 500_000;
const NONCE: u64 = 7;
const BASE_FEE: u64 = 10_000_000;
const CHAIN_ID: u64 = 42161;

fn addr(n: u64) -> Address {
    Address::left_padding_from(&n.to_be_bytes())
}

fn u(n: u64) -> U256 {
    U256::from(n)
}

fn tx_hash() -> TxHash {
    keccak256("tx")
}

struct Harness {
    mock: MockRpc,
    signer: Address,
    rounds: RoundsManagerInstance<DynProvider>,
    bonding: BondingManagerInstance<DynProvider>,
}

fn harness() -> Harness {
    let mock = MockRpc::new();
    // A fresh key per harness: the tests only need a signer to recover, never a fixed address.
    let key = PrivateKeySigner::random();
    let signer = key.address();
    let provider = signing_provider(mock.client(), key, CHAIN_ID);
    Harness {
        rounds: RoundsManager::new(addr(ROUNDS), provider.clone()),
        bonding: BondingManager::new(addr(BONDING), provider),
        signer,
        mock,
    }
}

fn cfg() -> Config {
    Config {
        http_rpc_url: "http://localhost:8545".into(),
        chain_id: CHAIN_ID,
        rounds_manager_addr: addr(ROUNDS),
        bonding_manager_addr: addr(BONDING),
        json_key_file: "key.json".into(),
        passphrase_file: "pass.txt".into(),
        orchestrator_addr: None,
        loop_sleep_secs: 6,
        receipt_timeout_secs: 90,
        max_fee_base_multiplier: 2,
        max_priority_fee_wei: U256::ZERO,
        enable_reward: true,
        enable_transfer_bond: true,
        lpt_receiver_addr: Some(addr(LPT_RECEIVER)),
        lpt_min_retain_wei: Some(u(1000)),
        enable_withdraw_fees: true,
        eth_fee_receiver_addr: Some(addr(ETH_RECEIVER)),
        eth_fee_withdraw_threshold_wei: Some(u(2000)),
    }
}

// -------------------------------------------------------------------
// Responses
// -------------------------------------------------------------------

impl Harness {
    fn round_state(&self, round: u64, initialized: bool, locked: bool) {
        self.mock.ret(u(round));
        self.mock.ret(initialized);
        self.mock.ret(locked);
    }

    /// getTranscoder() with the given lastRewardRound.
    fn transcoder(&self, last_reward_round: u64) {
        let mut t = [U256::ZERO; 10];
        t[0] = u(last_reward_round);
        self.mock.ret(t);
    }

    /// A tx that is sent and mined on the first receipt poll.
    fn mined(&self) {
        self.mock.sendable(GAS, NONCE, tx_hash());
        self.mock.receipt(tx_hash(), true, GAS / 2);
    }

    /// A tx that is sent but has no receipt yet (use with a zero receipt timeout).
    fn sent_not_mined(&self) {
        self.mock.sendable(GAS, NONCE, tx_hash());
        self.mock.no_receipt();
    }

    /// A tx that is sent, but polling for its receipt fails.
    fn sent_receipt_error(&self) {
        self.mock.sendable(GAS, NONCE, tx_hash());
        self.mock.err("eth_getTransactionReceipt");
    }

    /// A tx whose gas estimate fails, so it is never signed or sent.
    fn estimate_fails(&self) {
        self.mock.err("eth_estimateGas");
        // The nonce lookup runs alongside the estimate and may or may not be issued.
        self.mock
            .ok("eth_getTransactionCount", &format!("{NONCE:#x}"));
    }
}

// -------------------------------------------------------------------
// Request assertions
// -------------------------------------------------------------------

impl Harness {
    fn expect_round_state(&self) {
        self.mock.expect_call(&self.rounds.currentRound());
        self.mock
            .expect_call(&self.rounds.currentRoundInitialized());
        self.mock.expect_call(&self.rounds.currentRoundLocked());
    }

    /// Asserts the gas estimate, nonce lookup and signed send of `call` with the fees
    /// `with_l2_fees` sets, and returns the signed tx.
    #[track_caller]
    fn expect_send<P: Provider, D: CallDecoder>(
        &self,
        call: &CallBuilder<P, D>,
        max_fee: u128,
        tip: u128,
    ) -> TxEnvelope {
        let tx = self.mock.expect_signed_send();
        assert!(matches!(tx, TxEnvelope::Eip1559(_)), "{tx:?}");
        assert_eq!(Some(TxKind::Call(tx.to().unwrap())), call.as_ref().to);
        assert_eq!(tx.input(), call.calldata());
        assert_eq!(tx.max_fee_per_gas(), max_fee);
        assert_eq!(tx.max_priority_fee_per_gas(), Some(tip));
        assert_eq!(tx.gas_limit(), GAS);
        assert_eq!(tx.nonce(), NONCE);
        assert_eq!(tx.chain_id(), Some(CHAIN_ID));
        assert_eq!(tx.recover_signer().unwrap(), self.signer);
        tx
    }

    /// Asserts a failed gas estimate for `call` (and the nonce lookup, if it was issued).
    #[track_caller]
    fn expect_failed_estimate<P: Provider, D: CallDecoder>(&self, call: &CallBuilder<P, D>) {
        let params = self.mock.expect("eth_estimateGas");
        let input: Bytes = serde_json::from_value(params[0]["input"].clone()).unwrap();
        assert_eq!(&input, call.calldata());
        if let Some(next) = self.mock.peek_method()
            && next == "eth_getTransactionCount"
        {
            self.mock.expect("eth_getTransactionCount");
        }
    }

    fn expect_mined(&self) {
        self.mock.expect_receipt_poll(tx_hash());
    }
}

// -------------------------------------------------------------------
// fetch_round_state
// -------------------------------------------------------------------

#[tokio::test]
async fn fetch_round_state_reads_all_three_flags() {
    let h = harness();
    h.round_state(4371, true, false);
    let state = fetch_round_state(&h.rounds).await.unwrap();
    assert_eq!(
        state,
        RoundState {
            round: u(4371),
            initialized: true,
            locked: false,
        }
    );
    h.expect_round_state();
    h.mock.expect_no_more_requests();
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
        let results = [u(4371).abi_encode(), true.abi_encode(), true.abi_encode()];
        for r in &results[..fail_at] {
            h.mock.ok("eth_call", &Bytes::from(r.clone()));
        }
        h.mock.err("eth_call");

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

fn fees<P, D>(call: &CallBuilder<P, D>) -> (Option<u128>, Option<u128>) {
    let tx: &TransactionRequest = call.as_ref();
    (tx.max_fee_per_gas, tx.max_priority_fee_per_gas)
}

#[tokio::test]
async fn with_l2_fees_uses_base_fee_times_multiplier_plus_tip() {
    let h = harness();
    h.mock.latest_block(Some(BASE_FEE));
    let mut cfg = cfg();
    cfg.max_fee_base_multiplier = 3;
    cfg.max_priority_fee_wei = u(7);

    let call = h.bonding.rewardWithHint(addr(1), addr(2));
    let call = with_l2_fees(call, &cfg).await.unwrap();
    assert_eq!(fees(&call), (Some(3 * BASE_FEE as u128 + 7), Some(7)));
    // Calldata and target are untouched.
    assert_eq!(
        call.calldata(),
        h.bonding.rewardWithHint(addr(1), addr(2)).calldata()
    );
    let tx: &TransactionRequest = call.as_ref();
    assert_eq!(tx.to, Some(TxKind::Call(addr(BONDING))));
    h.mock.expect_latest_block();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn with_l2_fees_errors_without_latest_block() {
    let h = harness();
    h.mock.no_block();
    let err = with_l2_fees(h.bonding.reward(), &cfg()).await.unwrap_err();
    assert!(matches!(err, AppError::Provider(_)));
    assert_eq!(err.to_string(), "provider error: latest block not found");
}

#[tokio::test]
async fn with_l2_fees_errors_without_base_fee() {
    let h = harness();
    h.mock.latest_block(None);
    let err = with_l2_fees(h.bonding.reward(), &cfg()).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "provider error: latest block has no baseFeePerGas"
    );
}

#[tokio::test]
async fn with_l2_fees_reports_rpc_errors() {
    let h = harness();
    h.mock.err("eth_getBlockByNumber");
    let err = with_l2_fees(h.bonding.reward(), &cfg()).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("provider error: get_block(latest) failed:"),
        "{msg}"
    );
    assert!(msg.contains("boom"), "{msg}");
}

#[tokio::test]
async fn with_l2_fees_rejects_fees_above_u128() {
    let h = harness();
    h.mock.latest_block(Some(BASE_FEE));
    let cfg = Config {
        max_priority_fee_wei: U256::from(u128::MAX),
        ..cfg()
    };
    let err = with_l2_fees(h.bonding.reward(), &cfg).await.unwrap_err();
    assert!(matches!(err, AppError::Provider(_)), "{err}");
    let msg = err.to_string();
    assert!(msg.starts_with("provider error: maxFeePerGas "), "{msg}");
    assert!(msg.ends_with(" exceeds u128"), "{msg}");
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
        h.mock.latest_block(Some(base_fee));
        let mut cfg = cfg();
        cfg.max_fee_base_multiplier = multiplier;
        cfg.max_priority_fee_wei = u(tip);

        let call = rt
            .block_on(with_l2_fees(h.bonding.reward(), &cfg))
            .unwrap();
        // (2^64 - 1)^2 + (2^64 - 1) < 2^128, so u64 inputs always fit.
        let expected = base_fee as u128 * multiplier as u128 + tip as u128;
        prop_assert_eq!(fees(&call), (Some(expected), Some(tip as u128)));
    }
}

// -------------------------------------------------------------------
// maybe_reward_once_per_round
// -------------------------------------------------------------------

#[tokio::test]
async fn reward_skipped_when_already_rewarded_this_round() {
    for last_reward_round in [4371, 4372] {
        let h = harness();
        h.transcoder(last_reward_round);
        maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg())
            .await
            .unwrap();
        h.mock.expect_call(&h.bonding.getTranscoder(addr(ORCH)));
        h.mock.expect_no_more_requests();
    }
}

#[tokio::test]
async fn reward_reports_get_transcoder_failure() {
    let h = harness();
    h.mock.err("eth_call");
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg())
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
fn hint_responses(h: &Harness) {
    let m = &h.mock;
    m.ret(addr(0xc0));
    m.ret(addr(0xd0));
    let mut t = [U256::ZERO; 10];
    t[0] = u(4370);
    t[3] = u(4300);
    m.ret(t);
    m.ret([u(250), U256::ZERO, U256::ZERO, U256::ZERO, U256::ZERO]);
    m.ret(u(1000));
    m.ret(u(1000));
    m.ret(U256::from(10u64).pow(u(26)));
    m.ret(u(300));
    m.ret(u(100));
    m.ret(addr(HEAD));
    m.ret(u(500));
}

/// The eth_calls compute_reward_hints makes for [`hint_responses`].
fn expect_hint_calls(h: &Harness) {
    let provider = h.bonding.provider();
    let controller = Controller::new(addr(0xc0), provider);
    let minter = Minter::new(addr(0xd0), provider);
    let b = &h.bonding;
    let m = &h.mock;
    m.expect_call(&b.controller());
    m.expect_call(&controller.getContract(keccak256("Minter")));
    m.expect_call(&b.getTranscoder(addr(ORCH)));
    m.expect_call(&b.getTranscoderEarningsPoolForRound(addr(ORCH), u(4300)));
    m.expect_call(&minter.currentMintableTokens());
    m.expect_call(&b.currentRoundTotalActiveStake());
    m.expect_call(&b.treasuryRewardCutRate());
    m.expect_call(&b.transcoderTotalStake(addr(ORCH)));
    m.expect_call(&b.getTranscoderPoolMaxSize());
    m.expect_call(&b.getFirstTranscoderInPool());
    m.expect_call(&b.transcoderTotalStake(addr(HEAD)));
}

#[tokio::test]
async fn reward_sends_reward_with_hint_using_computed_hints() {
    let h = harness();
    h.transcoder(4370);
    hint_responses(&h);
    h.mock.latest_block(Some(BASE_FEE));
    h.mined();

    maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg())
        .await
        .unwrap();

    h.mock.expect_call(&h.bonding.getTranscoder(addr(ORCH)));
    expect_hint_calls(&h);
    h.mock.expect_latest_block();
    h.expect_send(
        &h.bonding.rewardWithHint(Address::ZERO, addr(HEAD)),
        2 * BASE_FEE as u128,
        0,
    );
    h.expect_mined();
    h.mock.expect_no_more_requests();
}

/// Responses for a reward that is needed, with hints failing at the first read, up to and
/// including the fee lookup.
fn reward_needed(h: &Harness) {
    h.transcoder(4370);
    h.mock.err("eth_call");
    h.mock.latest_block(Some(BASE_FEE));
}

#[tokio::test]
async fn reward_falls_back_to_zero_hints_when_hints_fail() {
    let h = harness();
    reward_needed(&h);
    h.mined();
    let mut cfg = cfg();
    cfg.max_priority_fee_wei = u(5);

    maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg)
        .await
        .unwrap();

    h.mock.expect_call(&h.bonding.getTranscoder(addr(ORCH)));
    h.mock.expect_call(&h.bonding.controller());
    h.mock.expect_latest_block();
    h.expect_send(
        &h.bonding.rewardWithHint(Address::ZERO, Address::ZERO),
        2 * BASE_FEE as u128 + 5,
        5,
    );
    h.expect_mined();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn reward_sends_signed_eip1559_tx_from_the_keystore_signer() {
    let h = harness();
    reward_needed(&h);
    h.mined();

    // The orchestrator is the signer, as in main() without ORCHESTRATOR_ADDR.
    maybe_reward_once_per_round(&h.bonding, h.signer, u(4371), &cfg())
        .await
        .unwrap();

    h.mock.expect_call(&h.bonding.getTranscoder(h.signer));
    h.mock.expect_call(&h.bonding.controller());
    h.mock.expect_latest_block();
    // expect_send checks the type, fields, chain id and recovered signer.
    let tx = h.expect_send(
        &h.bonding.rewardWithHint(Address::ZERO, Address::ZERO),
        2 * BASE_FEE as u128,
        0,
    );
    assert_eq!(tx.to(), Some(addr(BONDING)));
    assert_eq!(tx.value(), U256::ZERO);
    h.expect_mined();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn reward_fee_lookup_failure_is_returned() {
    let h = harness();
    h.transcoder(4370);
    h.mock.err("eth_call");
    h.mock.latest_block(None);
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg())
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Provider(_)), "{err}");
}

#[tokio::test]
async fn reward_send_failure_is_returned() {
    let h = harness();
    reward_needed(&h);
    h.estimate_fails();
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("tx error: rewardWithHint() send failed:"),
        "{msg}"
    );
    assert!(msg.contains("boom"), "{msg}");
}

#[tokio::test]
async fn reward_receipt_error_is_an_error() {
    let h = harness();
    reward_needed(&h);
    h.sent_receipt_error();
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with(&format!(
            "tx error: reward tx receipt error: tx_hash={:?} err=",
            tx_hash()
        )),
        "{msg}"
    );
    assert!(msg.contains("boom"), "{msg}");
}

#[tokio::test]
async fn reward_receipt_timeout_is_an_error() {
    let h = harness();
    reward_needed(&h);
    h.sent_not_mined();
    let mut cfg = cfg();
    cfg.receipt_timeout_secs = 0;
    let err = maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg)
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
async fn reward_waits_until_the_receipt_appears() {
    let h = harness();
    reward_needed(&h);
    h.mock.sendable(GAS, NONCE, tx_hash());
    h.mock.no_receipt();
    h.mock.no_receipt();
    h.mock.receipt(tx_hash(), true, GAS / 2);

    maybe_reward_once_per_round(&h.bonding, addr(ORCH), u(4371), &cfg())
        .await
        .unwrap();

    h.mock.expect_call(&h.bonding.getTranscoder(addr(ORCH)));
    h.mock.expect_call(&h.bonding.controller());
    h.mock.expect_latest_block();
    h.mock.expect_signed_send();
    for _ in 0..3 {
        h.mock.expect_receipt_poll(tx_hash());
    }
    h.mock.expect_no_more_requests();
}

// -------------------------------------------------------------------
// handle_locked_round_actions
// -------------------------------------------------------------------

async fn locked(
    h: &Harness,
    cfg: &Config,
    snapshot: &mut Option<LockedSnapshot>,
) -> Result<(), AppError> {
    handle_locked_round_actions(&h.bonding, addr(ORCH), u(4371), cfg, snapshot).await
}

fn snapshot(stake: Option<u64>, fees: Option<u64>) -> Option<LockedSnapshot> {
    Some(LockedSnapshot {
        round: u(4371),
        pending_stake: u(stake.unwrap_or(0)),
        pending_fees: u(fees.unwrap_or(0)),
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

/// Asserts the signed transferBond of `amount` to the LPT receiver, with zero hints.
fn expect_transfer_bond(h: &Harness, amount: u64) {
    let zero = Address::ZERO;
    h.expect_send(
        &h.bonding
            .transferBond(addr(LPT_RECEIVER), u(amount), zero, zero, zero, zero),
        2 * BASE_FEE as u128,
        0,
    );
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
    h.mock.expect_no_more_requests();
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
        h.mock.expect_no_more_requests();
    }
}

#[tokio::test]
async fn transfer_bond_skipped_when_nothing_above_retain() {
    // Below, equal to, and zero pending stake with retain = 1000.
    for pending in [999, 1000, 0] {
        let h = harness();
        h.mock.ret(u(pending));
        let mut snap = None;
        locked(&h, &only_transfer_bond(), &mut snap).await.unwrap();
        assert_eq!(snap, snapshot(Some(pending), None));
        h.mock
            .expect_call(&h.bonding.pendingStake(addr(ORCH), u(4371)));
        h.mock.expect_no_more_requests();
    }
}

#[tokio::test]
async fn transfer_bond_sends_stake_above_retain() {
    let h = harness();
    h.mock.ret(u(1500));
    h.mock.latest_block(Some(BASE_FEE));
    h.mined();
    let mut snap = None;
    locked(&h, &only_transfer_bond(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(Some(1500), None));

    h.mock
        .expect_call(&h.bonding.pendingStake(addr(ORCH), u(4371)));
    h.mock.expect_latest_block();
    expect_transfer_bond(&h, 500);
    h.expect_mined();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn transfer_bond_with_zero_retain_sends_everything() {
    let h = harness();
    h.mock.ret(u(1500));
    h.mock.latest_block(Some(BASE_FEE));
    h.mined();
    let cfg = Config {
        lpt_min_retain_wei: Some(U256::ZERO),
        ..only_transfer_bond()
    };
    locked(&h, &cfg, &mut None).await.unwrap();

    h.mock
        .expect_call(&h.bonding.pendingStake(addr(ORCH), u(4371)));
    h.mock.expect_latest_block();
    expect_transfer_bond(&h, 1500);
}

/// Scripts a tx that goes wrong after the fee lookup.
type TxProblem = fn(&Harness);

/// Send failure, receipt poll error, receipt timeout (each with its receipt timeout): all end
/// in Ok with the snapshot updated.
fn tx_problem_cases() -> [(TxProblem, u64); 3] {
    [
        (Harness::estimate_fails, 90),
        (Harness::sent_receipt_error, 90),
        (Harness::sent_not_mined, 0),
    ]
}

#[tokio::test]
async fn transfer_bond_tx_problems_are_logged_not_returned() {
    for (tx_problem, receipt_timeout_secs) in tx_problem_cases() {
        let h = harness();
        h.mock.ret(u(1500));
        h.mock.latest_block(Some(BASE_FEE));
        tx_problem(&h);
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
    // pendingStake, then pendingFees.
    h.mock.ret(u(1500));
    h.mock.ret(u(3000));
    h.mock.latest_block(Some(BASE_FEE));
    h.mock.latest_block(Some(BASE_FEE));
    // transferBond's estimate fails; withdrawFees is mined.
    h.mock.err("eth_estimateGas");
    h.mined();
    let mut snap = None;
    locked(&h, &cfg(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(Some(1500), Some(3000)));

    h.mock
        .expect_call(&h.bonding.pendingStake(addr(ORCH), u(4371)));
    h.mock.expect_latest_block();
    let zero = Address::ZERO;
    h.expect_failed_estimate(&h.bonding.transferBond(
        addr(LPT_RECEIVER),
        u(500),
        zero,
        zero,
        zero,
        zero,
    ));
    h.mock
        .expect_call(&h.bonding.pendingFees(addr(ORCH), u(4371)));
    h.mock.expect_latest_block();
    h.expect_send(
        &h.bonding.withdrawFees(addr(ETH_RECEIVER), u(3000)),
        2 * BASE_FEE as u128,
        0,
    );
    h.expect_mined();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn every_send_uses_the_nodes_pending_nonce() {
    // The node still reports nonce 7 after the first send (say it never landed). The second tx
    // must reuse 7; a locally cached nonce would sign 8 and leave a gap that stalls every tx.
    let h = harness();
    h.mock.ret(u(1500));
    h.mock.ret(u(3000));
    h.mock.latest_block(Some(BASE_FEE));
    h.mock.latest_block(Some(BASE_FEE));
    h.mined();
    h.mined();
    locked(&h, &cfg(), &mut None).await.unwrap();

    h.mock
        .expect_call(&h.bonding.pendingStake(addr(ORCH), u(4371)));
    h.mock.expect_latest_block();
    expect_transfer_bond(&h, 500);
    h.expect_mined();
    h.mock
        .expect_call(&h.bonding.pendingFees(addr(ORCH), u(4371)));
    h.mock.expect_latest_block();
    // expect_send asserts the nonce lookup was made and nonce == NONCE.
    h.expect_send(
        &h.bonding.withdrawFees(addr(ETH_RECEIVER), u(3000)),
        2 * BASE_FEE as u128,
        0,
    );
    h.expect_mined();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn transfer_bond_fee_lookup_failure_aborts_locked_actions() {
    let h = harness();
    h.mock.ret(u(1500));
    h.mock.latest_block(None);
    let mut snap = None;
    let err = locked(&h, &cfg(), &mut snap).await.unwrap_err();
    assert!(matches!(err, AppError::Provider(_)), "{err}");
    // withdrawFees is not attempted and the snapshot is not updated.
    assert_eq!(snap, None);
    h.mock
        .expect_call(&h.bonding.pendingStake(addr(ORCH), u(4371)));
    h.mock.expect_latest_block();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn pending_stake_failure_is_returned() {
    let h = harness();
    h.mock.err("eth_call");
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
    h.mock.ret(U256::ZERO);
    h.mock.err("eth_call");
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
    h.mock.ret(u(1999));
    let mut snap = None;
    locked(&h, &only_withdraw_fees(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(None, Some(1999)));
    h.mock
        .expect_call(&h.bonding.pendingFees(addr(ORCH), u(4371)));
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn withdraw_fees_skipped_when_zero_even_with_zero_threshold() {
    let h = harness();
    h.mock.ret(U256::ZERO);
    let cfg = Config {
        eth_fee_withdraw_threshold_wei: Some(U256::ZERO),
        ..only_withdraw_fees()
    };
    locked(&h, &cfg, &mut None).await.unwrap();
    h.mock
        .expect_call(&h.bonding.pendingFees(addr(ORCH), u(4371)));
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn withdraw_fees_sends_all_pending_fees_at_threshold() {
    let h = harness();
    h.mock.ret(u(2000));
    h.mock.latest_block(Some(BASE_FEE));
    h.mined();
    let mut snap = None;
    locked(&h, &only_withdraw_fees(), &mut snap).await.unwrap();
    assert_eq!(snap, snapshot(None, Some(2000)));

    h.mock
        .expect_call(&h.bonding.pendingFees(addr(ORCH), u(4371)));
    h.mock.expect_latest_block();
    h.expect_send(
        &h.bonding.withdrawFees(addr(ETH_RECEIVER), u(2000)),
        2 * BASE_FEE as u128,
        0,
    );
    h.expect_mined();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn withdraw_fees_tx_problems_are_logged_not_returned() {
    for (tx_problem, receipt_timeout_secs) in tx_problem_cases() {
        let h = harness();
        h.mock.ret(u(5000));
        h.mock.latest_block(Some(BASE_FEE));
        tx_problem(&h);
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
    h.mock.ret(u(5000));
    h.mock.latest_block(None);
    let err = locked(&h, &only_withdraw_fees(), &mut None)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Provider(_)), "{err}");
}

#[tokio::test]
async fn locked_snapshot_tracks_changes() {
    let h = harness();
    // Same values twice, then fees change.
    for v in [10, 20, 10, 20, 10, 30] {
        h.mock.ret(u(v));
    }
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
        round: u(round),
        initialized,
        locked,
    })
}

#[tokio::test]
async fn poll_keeps_last_state_when_round_state_fetch_fails() {
    let h = harness();
    h.mock.err("eth_call");
    let mut last = state(4370, true, true);
    poll(&h, &cfg(), &mut last, &mut None).await;
    assert_eq!(last, state(4370, true, true));
    h.mock.expect_call(&h.rounds.currentRound());
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn poll_checks_reward_when_initialized() {
    let h = harness();
    h.round_state(4371, true, false);
    h.transcoder(4371);
    let mut last = None;
    let mut snap = None;
    poll(&h, &cfg(), &mut last, &mut snap).await;
    assert_eq!(last, state(4371, true, false));
    assert_eq!(snap, None);
    h.expect_round_state();
    h.mock.expect_call(&h.bonding.getTranscoder(addr(ORCH)));
    h.mock.expect_no_more_requests();
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
        h.round_state(4371, initialized, false);
        let mut last = state(4371, initialized, false);
        poll(&h, &cfg, &mut last, &mut None).await;
        assert_eq!(last, state(4371, initialized, false));
        h.expect_round_state();
        h.mock.expect_no_more_requests();
    }
}

#[tokio::test]
async fn poll_runs_locked_actions_when_locked() {
    let h = harness();
    // Initialized and locked: reward check first, then both locked-round reads.
    h.round_state(4371, true, true);
    h.transcoder(4371);
    h.mock.ret(U256::ZERO);
    h.mock.ret(U256::ZERO);
    let mut last = state(4371, true, false);
    let mut snap = None;
    poll(&h, &cfg(), &mut last, &mut snap).await;
    assert_eq!(last, state(4371, true, true));
    assert_eq!(snap, snapshot(Some(0), Some(0)));
    h.expect_round_state();
    h.mock.expect_call(&h.bonding.getTranscoder(addr(ORCH)));
    h.mock
        .expect_call(&h.bonding.pendingStake(addr(ORCH), u(4371)));
    h.mock
        .expect_call(&h.bonding.pendingFees(addr(ORCH), u(4371)));
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn poll_skips_locked_actions_when_disabled() {
    let h = harness();
    h.round_state(4371, false, true);
    let cfg = Config {
        enable_transfer_bond: false,
        enable_withdraw_fees: false,
        ..cfg()
    };
    let mut snap = None;
    poll(&h, &cfg, &mut None, &mut snap).await;
    assert_eq!(snap, None);
    h.expect_round_state();
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn poll_logs_action_errors_and_still_records_state() {
    let h = harness();
    // getTranscoder fails, pendingStake fails.
    h.round_state(4371, true, true);
    h.mock.err("eth_call");
    h.mock.err("eth_call");
    let mut last = None;
    let mut snap = None;
    poll(&h, &cfg(), &mut last, &mut snap).await;
    assert_eq!(last, state(4371, true, true));
    assert_eq!(snap, None);
    h.expect_round_state();
    h.mock.expect_call(&h.bonding.getTranscoder(addr(ORCH)));
    h.mock
        .expect_call(&h.bonding.pendingStake(addr(ORCH), u(4371)));
    h.mock.expect_no_more_requests();
}

// -------------------------------------------------------------------
// check_transfer_bond_authorization
// -------------------------------------------------------------------

#[tokio::test]
async fn authorization_check_asks_if_receiver_authorized_signer() {
    for (authorized, expected) in [
        (true, BondAuthorization::Authorized),
        (false, BondAuthorization::NotAuthorized),
    ] {
        let h = harness();
        h.mock.ret(authorized);
        let got = check_transfer_bond_authorization(&h.bonding, h.signer, &cfg()).await;
        assert_eq!(got, expected);
        // The receiver is the delegator; the signer (the transferBond sender) is the bonder.
        h.mock.expect_call(
            &h.bonding
                .isThirdPartyBonderAuthorized(addr(LPT_RECEIVER), h.signer),
        );
        h.mock.expect_no_more_requests();
    }
}

#[tokio::test]
async fn authorization_check_failure_does_not_stop_startup() {
    let h = harness();
    h.mock.err("eth_call");
    let got = check_transfer_bond_authorization(&h.bonding, h.signer, &cfg()).await;
    assert_eq!(got, BondAuthorization::Unknown);
    h.mock.expect_call(
        &h.bonding
            .isThirdPartyBonderAuthorized(addr(LPT_RECEIVER), h.signer),
    );
    h.mock.expect_no_more_requests();
}

#[tokio::test]
async fn authorization_not_checked_without_a_third_party_transfer() {
    let h = harness();
    let disabled = Config {
        enable_transfer_bond: false,
        ..cfg()
    };
    let to_self = Config {
        lpt_receiver_addr: Some(h.signer),
        ..cfg()
    };
    for cfg in [disabled, to_self] {
        let got = check_transfer_bond_authorization(&h.bonding, h.signer, &cfg).await;
        assert_eq!(got, BondAuthorization::NotNeeded);
    }
    h.mock.expect_no_more_requests();
}
