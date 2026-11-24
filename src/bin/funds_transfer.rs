use std::{path::Path, time::Duration};

use alloy::{
    contract::{CallBuilder, CallDecoder},
    eips::BlockNumberOrTag,
    primitives::{Address, TxHash, U256},
    providers::Provider,
    rpc::client::RpcClient,
    signers::local::PrivateKeySigner,
};
use livepeer_funds_transfer::{
    client::{signing_provider, wait_for_receipt},
    config::{AppError, Config, load_config, validate_config},
    contracts::{BondingManager, BondingManagerInstance, RoundsManager, RoundsManagerInstance},
    reward_hints::compute_reward_hints,
};
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

#[cfg(test)]
#[path = "../mock_rpc.rs"]
mod mock_rpc;
#[cfg(test)]
#[path = "funds_transfer/tests.rs"]
mod tests;

/// How often to poll for a sent tx's receipt (Arbitrum mines within about a second).
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RoundState {
    round: U256,
    initialized: bool,
    locked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LockedSnapshot {
    round: U256,
    pending_stake: U256,
    pending_fees: U256,
    stake_present: bool,
    fees_present: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let cfg = load_config()?;
    validate_config(&cfg)?;

    info!(
        "starting funds_transfer: chain_id={} rounds_manager={:?} bonding_manager={:?} sleep_secs={} flags(reward={}, transfer_bond={}, withdraw_fees={})",
        cfg.chain_id,
        cfg.rounds_manager_addr,
        cfg.bonding_manager_addr,
        cfg.loop_sleep_secs,
        cfg.enable_reward,
        cfg.enable_transfer_bond,
        cfg.enable_withdraw_fees
    );

    let rpc_url = cfg
        .http_rpc_url
        .parse()
        .map_err(|e| AppError::Provider(format!("invalid HTTP_RPC_URL: {e}")))?;

    // load wallet (keystore + passphrase files)
    let passphrase = std::fs::read_to_string(&cfg.passphrase_file)
        .map_err(|e| AppError::Wallet(format!("failed to read PASSPHRASE_FILE: {e}")))?;
    let passphrase = passphrase.trim_end();

    let key_json_path = Path::new(&cfg.json_key_file);
    let signer = PrivateKeySigner::decrypt_keystore(key_json_path, passphrase)
        .map_err(|e| AppError::Wallet(format!("failed to decrypt JSON_KEY_FILE: {e}")))?;

    let signer_addr = signer.address();
    let orchestrator_addr = cfg.orchestrator_addr.unwrap_or(signer_addr);

    if orchestrator_addr != signer_addr {
        warn!(
            "ORCHESTRATOR_ADDR differs from signer address; using orchestrator={:?} signer={:?}",
            orchestrator_addr, signer_addr
        );
    } else {
        info!("orchestrator/signer address: {:?}", orchestrator_addr);
    }

    let provider = signing_provider(RpcClient::new_http(rpc_url), signer, cfg.chain_id);
    let rounds = RoundsManager::new(cfg.rounds_manager_addr, provider.clone());
    let bonding = BondingManager::new(cfg.bonding_manager_addr, provider);

    // Before the loop, so a missing authorization shows up at startup rather than as failing
    // transfers at the next locked round.
    check_transfer_bond_authorization(&bonding, signer_addr, &cfg).await;

    let mut last_state: Option<RoundState> = None;
    let mut last_locked_snapshot: Option<LockedSnapshot> = None;

    loop {
        poll_once(
            &rounds,
            &bonding,
            orchestrator_addr,
            &cfg,
            &mut last_state,
            &mut last_locked_snapshot,
        )
        .await;
        sleep(Duration::from_secs(cfg.loop_sleep_secs)).await;
    }
}

/// Outcome of the startup third-party bonder authorization check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BondAuthorization {
    /// transferBond is disabled, or the receiver is the signer itself (not a third party).
    NotNeeded,
    Authorized,
    NotAuthorized,
    /// The check itself failed: an RPC error, or a BondingManager without the registry.
    Unknown,
}

/// Checks whether LPT_RECEIVER_ADDR has authorized the signer as a third-party bonder.
///
/// The signer sends transferBond, which bonds stake to the receiver: that makes the signer a
/// third-party bonder for the receiver. Once the protocol enforces third-party bonding
/// authorization, the transfer only succeeds if the receiver has called
/// `setThirdPartyBonderAuthorization(signer, true)`. The outcome is logged for the operator.
/// It never stops the app, because rewards and fee withdrawals do not depend on it.
async fn check_transfer_bond_authorization<P: Provider>(
    bonding: &BondingManagerInstance<P>,
    signer: Address,
    cfg: &Config,
) -> BondAuthorization {
    let receiver = match cfg.lpt_receiver_addr {
        Some(receiver) if cfg.enable_transfer_bond && receiver != signer => receiver,
        _ => return BondAuthorization::NotNeeded,
    };
    match bonding
        .isThirdPartyBonderAuthorized(receiver, signer)
        .call()
        .await
    {
        Ok(true) => {
            info!(
                "transferBond authorization OK: LPT_RECEIVER_ADDR={receiver:?} has authorized signer={signer:?} as a third-party bonder"
            );
            BondAuthorization::Authorized
        }
        Ok(false) => {
            warn!(
                "transferBond authorization MISSING: LPT_RECEIVER_ADDR={receiver:?} has not authorized signer={signer:?} as a third-party bonder. \
                 Once the protocol enforces third-party bonding authorization, every transferBond will fail. \
                 Fix: from {receiver:?}, call setThirdPartyBonderAuthorization({signer:?}, true) on BondingManager {:?}. \
                 Rewards and fee withdrawals are not affected.",
                bonding.address()
            );
            BondAuthorization::NotAuthorized
        }
        Err(e) => {
            warn!("transferBond authorization check failed: {e}; continuing without it");
            BondAuthorization::Unknown
        }
    }
}

/// One iteration of the main loop: read the round state, then run whatever actions it calls
/// for. Errors are logged, never returned; the next iteration re-checks and retries.
async fn poll_once<P: Provider>(
    rounds: &RoundsManagerInstance<P>,
    bonding: &BondingManagerInstance<P>,
    orchestrator_addr: Address,
    cfg: &Config,
    last_state: &mut Option<RoundState>,
    last_locked_snapshot: &mut Option<LockedSnapshot>,
) {
    let state = match fetch_round_state(rounds).await {
        Ok(s) => s,
        Err(e) => {
            warn!("failed to fetch round state: {e}; will retry next loop");
            return;
        }
    };

    let state_changed = last_state.map(|ls| ls != state).unwrap_or(true);
    if state_changed {
        info!(
            "round state changed: round={} initialized={} locked={}",
            state.round, state.initialized, state.locked
        );
    } else {
        debug!(
            "round unchanged: round={} initialized={} locked={}",
            state.round, state.initialized, state.locked
        );
    }

    // 1) When initialized: reward() once per round
    if cfg.enable_reward
        && state.initialized
        && let Err(e) =
            maybe_reward_once_per_round(bonding, orchestrator_addr, state.round, cfg).await
    {
        // no internal retries: next loop will re-check and retry if still needed
        warn!("reward check/tx failed: {e}; will retry next loop if still needed");
    }

    // 2) When locked: transferBond + withdrawFees
    if (cfg.enable_transfer_bond || cfg.enable_withdraw_fees)
        && state.locked
        && let Err(e) = handle_locked_round_actions(
            bonding,
            orchestrator_addr,
            state.round,
            cfg,
            last_locked_snapshot,
        )
        .await
    {
        warn!("locked-round actions failed: {e}; will retry next loop if still needed");
    }

    *last_state = Some(state);
}

async fn fetch_round_state<P: Provider>(
    rounds: &RoundsManagerInstance<P>,
) -> Result<RoundState, AppError> {
    let round = rounds
        .currentRound()
        .call()
        .await
        .map_err(|e| AppError::Contract(format!("RoundsManager.currentRound() failed: {e}")))?;

    let initialized = rounds.currentRoundInitialized().call().await.map_err(|e| {
        AppError::Contract(format!(
            "RoundsManager.currentRoundInitialized() failed: {e}"
        ))
    })?;

    let locked = rounds.currentRoundLocked().call().await.map_err(|e| {
        AppError::Contract(format!("RoundsManager.currentRoundLocked() failed: {e}"))
    })?;

    Ok(RoundState {
        round,
        initialized,
        locked,
    })
}

/// Calls bonding.rewardWithHint() ONLY if lastRewardRound < current_round.
/// No internal retries: if tx fails, caller logs and next loop will retry.
async fn maybe_reward_once_per_round<P: Provider>(
    bonding: &BondingManagerInstance<P>,
    orchestrator: Address,
    current_round: U256,
    cfg: &Config,
) -> Result<(), AppError> {
    let receipt_timeout_secs = cfg.receipt_timeout_secs;
    let last_reward_round = bonding
        .getTranscoder(orchestrator)
        .call()
        .await
        .map_err(|e| AppError::Contract(format!("BondingManager.getTranscoder() failed: {e}")))?
        .lastRewardRound;

    if last_reward_round >= current_round {
        debug!(
            "reward not needed: lastRewardRound={} currentRound={}",
            last_reward_round, current_round
        );
        return Ok(());
    }

    info!(
        "reward needed: lastRewardRound={} < currentRound={} (sending reward tx)",
        last_reward_round, current_round
    );

    // Hints only save gas; zero hints make rewardWithHint behave exactly like reward().
    let (prev, next) = match compute_reward_hints(bonding, orchestrator, current_round).await {
        Ok(h) => {
            info!(
                "reward hints: prev={:?} next={:?} stakeWei={} expectedRewardWei={}",
                h.prev, h.next, h.current_stake, h.expected_reward
            );
            (h.prev, h.next)
        }
        Err(e) => {
            warn!("reward hint computation failed: {e}; sending rewardWithHint without hints");
            (Address::ZERO, Address::ZERO)
        }
    };

    let call = with_l2_fees(bonding.rewardWithHint(prev, next), cfg).await?;
    let pending = call
        .send()
        .await
        .map_err(|e| AppError::Tx(format!("rewardWithHint() send failed: {e}")))?;

    let tx_hash: TxHash = *pending.tx_hash();
    info!("reward tx sent: tx_hash={:?}", tx_hash);

    // Wait for receipt with a timeout so we don't hang forever.
    match timeout(
        Duration::from_secs(receipt_timeout_secs),
        wait_for_receipt(bonding.provider(), tx_hash, RECEIPT_POLL_INTERVAL),
    )
    .await
    {
        Ok(Ok(receipt)) => {
            info!(
                "reward tx confirmed: tx_hash={:?} success={} block={:?} gas_used={}",
                receipt.transaction_hash,
                receipt.status(),
                receipt.block_number,
                receipt.gas_used
            );
            Ok(())
        }
        Ok(Err(e)) => Err(AppError::Tx(format!(
            "reward tx receipt error: tx_hash={:?} err={e}",
            tx_hash
        ))),
        Err(_) => Err(AppError::Tx(format!(
            "reward tx receipt timeout after {}s: tx_hash={:?}",
            receipt_timeout_secs, tx_hash
        ))),
    }
}

/// Sets EIP-1559 fees from the latest base fee instead of a client-side fee estimator.
///
/// Mainnet-oriented estimators (ethers used a 3 gwei minimum tip) inflate maxFeePerGas by 100x+
/// on Arbitrum (base fee ~0.01 gwei). eth_estimateGas caps gas at balance / maxFeePerGas, so a
/// modestly funded wallet intermittently fails with "gas required exceeds allowance". Arbitrum
/// ignores the tip, so a zero tip is fine. With both fees set, the provider only estimates gas.
async fn with_l2_fees<P: Provider, D: CallDecoder>(
    call: CallBuilder<P, D>,
    cfg: &Config,
) -> Result<CallBuilder<P, D>, AppError> {
    let block = call
        .provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await
        .map_err(|e| AppError::Provider(format!("get_block(latest) failed: {e}")))?
        .ok_or_else(|| AppError::Provider("latest block not found".into()))?;
    let base_fee = block
        .header
        .base_fee_per_gas
        .ok_or_else(|| AppError::Provider("latest block has no baseFeePerGas".into()))?;

    let priority_fee = cfg.max_priority_fee_wei;
    let max_fee = U256::from(base_fee) * U256::from(cfg.max_fee_base_multiplier) + priority_fee;

    debug!(
        "tx fees: baseFeeWei={} maxFeePerGasWei={} maxPriorityFeePerGasWei={}",
        base_fee, max_fee, priority_fee
    );

    // alloy carries fees as u128; anything larger is not a real fee.
    let to_u128 = |name: &str, v: U256| {
        u128::try_from(v).map_err(|_| AppError::Provider(format!("{name} {v} exceeds u128")))
    };
    Ok(call
        .max_fee_per_gas(to_u128("maxFeePerGas", max_fee)?)
        .max_priority_fee_per_gas(to_u128("maxPriorityFeePerGas", priority_fee)?))
}

async fn handle_locked_round_actions<P: Provider>(
    bonding: &BondingManagerInstance<P>,
    orchestrator: Address,
    current_round: U256,
    cfg: &Config,
    last_locked_snapshot: &mut Option<LockedSnapshot>,
) -> Result<(), AppError> {
    if !cfg.enable_transfer_bond && !cfg.enable_withdraw_fees {
        debug!("locked-round actions skipped: both transferBond and withdrawFees are disabled");
        return Ok(());
    }

    let mut pending_stake_for_snapshot: Option<U256> = None;
    let mut pending_fees_for_snapshot: Option<U256> = None;

    // ----------------------------
    // transferBond (optional)
    // ----------------------------
    if cfg.enable_transfer_bond {
        let receiver = cfg.lpt_receiver_addr.ok_or_else(|| {
            AppError::BadEnv(
                "LPT_RECEIVER_ADDR",
                "required when ENABLE_TRANSFER_BOND=true".into(),
            )
        })?;
        let retain = cfg.lpt_min_retain_wei.ok_or_else(|| {
            AppError::BadEnv(
                "LPT_MIN_RETAIN_WEI",
                "required when ENABLE_TRANSFER_BOND=true".into(),
            )
        })?;

        let pending_stake = bonding
            .pendingStake(orchestrator, current_round)
            .call()
            .await
            .map_err(|e| AppError::Contract(format!("pendingStake() failed: {e}")))?;

        pending_stake_for_snapshot = Some(pending_stake);

        // Keep check logs at DEBUG to avoid redundant INFO spam.
        debug!(
            "transferBond check: round={} pendingStakeWei={} retainWei={}",
            current_round, pending_stake, retain
        );

        let transferable = match pending_stake.checked_sub(retain) {
            Some(v) if !v.is_zero() => v,
            _ => {
                debug!(
                    "transferBond skipped: pendingStakeWei={} <= retainWei={} round={}",
                    pending_stake, retain, current_round
                );
                U256::ZERO
            }
        };

        if !transferable.is_zero() {
            info!(
                "transferBond sending: round={} from_orchestrator={:?} to_receiver={:?} amountWei={}",
                current_round, orchestrator, receiver, transferable
            );

            let call = with_l2_fees(
                bonding.transferBond(
                    receiver,
                    transferable,
                    Address::ZERO,
                    Address::ZERO,
                    Address::ZERO,
                    Address::ZERO,
                ),
                cfg,
            )
            .await?;

            match call.send().await {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    info!(
                        "transferBond tx sent: round={} tx_hash={:?}",
                        current_round, tx_hash
                    );

                    match timeout(
                        Duration::from_secs(cfg.receipt_timeout_secs),
                        wait_for_receipt(bonding.provider(), tx_hash, RECEIPT_POLL_INTERVAL),
                    )
                    .await
                    {
                        Ok(Ok(receipt)) => {
                            info!(
                                "transferBond confirmed: round={} tx_hash={:?} success={} block={:?} gas_used={}",
                                current_round,
                                receipt.transaction_hash,
                                receipt.status(),
                                receipt.block_number,
                                receipt.gas_used
                            );
                        }
                        Ok(Err(e)) => {
                            warn!(
                                "transferBond receipt error: round={} tx_hash={:?} err={}",
                                current_round, tx_hash, e
                            );
                        }
                        Err(_) => {
                            warn!(
                                "transferBond receipt timeout after {}s: round={} tx_hash={:?}",
                                cfg.receipt_timeout_secs, current_round, tx_hash
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "transferBond send failed: round={} to_receiver={:?} amountWei={} err={}",
                        current_round, receiver, transferable, e
                    );
                }
            }
        }
    }

    // ----------------------------
    // withdrawFees (optional)
    // ----------------------------
    if cfg.enable_withdraw_fees {
        let receiver = cfg.eth_fee_receiver_addr.ok_or_else(|| {
            AppError::BadEnv(
                "ETH_FEE_RECEIVER_ADDR",
                "required when ENABLE_WITHDRAW_FEES=true".into(),
            )
        })?;
        let threshold = cfg.eth_fee_withdraw_threshold_wei.ok_or_else(|| {
            AppError::BadEnv(
                "ETH_FEE_WITHDRAW_THRESHOLD_WEI",
                "required when ENABLE_WITHDRAW_FEES=true".into(),
            )
        })?;

        let pending_fees = bonding
            .pendingFees(orchestrator, current_round)
            .call()
            .await
            .map_err(|e| AppError::Contract(format!("pendingFees() failed: {e}")))?;

        pending_fees_for_snapshot = Some(pending_fees);

        debug!(
            "withdrawFees check: round={} pendingFeesWei={} thresholdWei={}",
            current_round, pending_fees, threshold
        );

        if pending_fees >= threshold && !pending_fees.is_zero() {
            info!(
                "withdrawFees sending: round={} from_orchestrator={:?} to_receiver={:?} amountWei={}",
                current_round, orchestrator, receiver, pending_fees
            );

            let call = with_l2_fees(bonding.withdrawFees(receiver, pending_fees), cfg).await?;

            match call.send().await {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    info!(
                        "withdrawFees tx sent: round={} tx_hash={:?}",
                        current_round, tx_hash
                    );

                    match timeout(
                        Duration::from_secs(cfg.receipt_timeout_secs),
                        wait_for_receipt(bonding.provider(), tx_hash, RECEIPT_POLL_INTERVAL),
                    )
                    .await
                    {
                        Ok(Ok(receipt)) => {
                            info!(
                                "withdrawFees confirmed: round={} tx_hash={:?} success={} block={:?} gas_used={}",
                                current_round,
                                receipt.transaction_hash,
                                receipt.status(),
                                receipt.block_number,
                                receipt.gas_used
                            );
                        }
                        Ok(Err(e)) => {
                            warn!(
                                "withdrawFees receipt error: round={} tx_hash={:?} err={}",
                                current_round, tx_hash, e
                            );
                        }
                        Err(_) => {
                            warn!(
                                "withdrawFees receipt timeout after {}s: round={} tx_hash={:?}",
                                cfg.receipt_timeout_secs, current_round, tx_hash
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "withdrawFees send failed: round={} to_receiver={:?} amountWei={} err={}",
                        current_round, receiver, pending_fees, e
                    );
                }
            }
        } else {
            debug!(
                "withdrawFees skipped: round={} pendingFeesWei={} < thresholdWei={}",
                current_round, pending_fees, threshold
            );
        }
    }

    // Emit a single INFO snapshot when the locked-round values change (not every loop).
    let snap = LockedSnapshot {
        round: current_round,
        pending_stake: pending_stake_for_snapshot.unwrap_or_default(),
        pending_fees: pending_fees_for_snapshot.unwrap_or_default(),
        stake_present: pending_stake_for_snapshot.is_some(),
        fees_present: pending_fees_for_snapshot.is_some(),
    };

    if *last_locked_snapshot != Some(snap) {
        if snap.stake_present && snap.fees_present {
            info!(
                "locked snapshot changed: round={} pendingStakeWei={} pendingFeesWei={}",
                snap.round, snap.pending_stake, snap.pending_fees
            );
        } else if snap.stake_present {
            info!(
                "locked snapshot changed: round={} pendingStakeWei={}",
                snap.round, snap.pending_stake
            );
        } else if snap.fees_present {
            info!(
                "locked snapshot changed: round={} pendingFeesWei={}",
                snap.round, snap.pending_fees
            );
        } else {
            info!("locked snapshot changed: round={}", snap.round);
        }

        *last_locked_snapshot = Some(snap);
    } else {
        debug!(
            "locked snapshot unchanged: round={} stake_present={} fees_present={}",
            snap.round, snap.stake_present, snap.fees_present
        );
    }

    Ok(())
}

fn init_logging() {
    let filter = match tracing_subscriber::EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => tracing_subscriber::EnvFilter::new("info"),
    };

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
