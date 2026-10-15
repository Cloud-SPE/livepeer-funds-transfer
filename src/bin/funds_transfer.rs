use std::{path::Path, sync::Arc, time::Duration};

use ethers::{
    contract::ContractCall,
    middleware::SignerMiddleware,
    providers::{Http, Middleware, Provider},
    signers::{LocalWallet, Signer},
    types::{Address, BlockNumber, TxHash, U256, transaction::eip2718::TypedTransaction},
};
use livepeer_funds_transfer::{
    config::{AppError, Config, load_config, validate_config},
    contracts::{BondingManager, RoundsManager},
    reward_hints::compute_reward_hints,
};
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

#[cfg(test)]
#[path = "funds_transfer/tests.rs"]
mod tests;

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

    let provider = Provider::<Http>::try_from(cfg.http_rpc_url.as_str())
        .map_err(|e| AppError::Provider(format!("{e}")))?;
    // internal polling interval for provider housekeeping
    let provider = provider.interval(Duration::from_millis(250));

    // load wallet (keystore + passphrase files)
    let passphrase = std::fs::read_to_string(&cfg.passphrase_file)
        .map_err(|e| AppError::Wallet(format!("failed to read PASSPHRASE_FILE: {e}")))?;
    let passphrase = passphrase.trim_end();

    let key_json_path = Path::new(&cfg.json_key_file);
    let wallet = LocalWallet::decrypt_keystore(key_json_path, passphrase)
        .map_err(|e| AppError::Wallet(format!("failed to decrypt JSON_KEY_FILE: {e}")))?
        .with_chain_id(cfg.chain_id);

    let signer_addr = wallet.address();
    let orchestrator_addr = cfg.orchestrator_addr.unwrap_or(signer_addr);

    if orchestrator_addr != signer_addr {
        warn!(
            "ORCHESTRATOR_ADDR differs from signer address; using orchestrator={:?} signer={:?}",
            orchestrator_addr, signer_addr
        );
    } else {
        info!("orchestrator/signer address: {:?}", orchestrator_addr);
    }

    let client = Arc::new(SignerMiddleware::new(provider, wallet));
    let rounds = RoundsManager::new(cfg.rounds_manager_addr, client.clone());
    let bonding = BondingManager::new(cfg.bonding_manager_addr, client.clone());

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

/// One iteration of the main loop: read the round state, then run whatever actions it calls
/// for. Errors are logged, never returned; the next iteration re-checks and retries.
async fn poll_once<M: Middleware>(
    rounds: &RoundsManager<M>,
    bonding: &BondingManager<M>,
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

async fn fetch_round_state<M: Middleware>(
    rounds: &RoundsManager<M>,
) -> Result<RoundState, AppError> {
    let round = rounds
        .current_round()
        .call()
        .await
        .map_err(|e| AppError::Contract(format!("RoundsManager.currentRound() failed: {e}")))?;

    let initialized = rounds
        .current_round_initialized()
        .call()
        .await
        .map_err(|e| {
            AppError::Contract(format!(
                "RoundsManager.currentRoundInitialized() failed: {e}"
            ))
        })?;

    let locked = rounds.current_round_locked().call().await.map_err(|e| {
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
async fn maybe_reward_once_per_round<M: Middleware>(
    bonding: &BondingManager<M>,
    orchestrator: Address,
    current_round: U256,
    cfg: &Config,
) -> Result<(), AppError> {
    let receipt_timeout_secs = cfg.receipt_timeout_secs;
    // getTranscoder(addr) returns a tuple whose first element is lastRewardRound (per ABI)
    let t = bonding
        .get_transcoder(orchestrator)
        .call()
        .await
        .map_err(|e| AppError::Contract(format!("BondingManager.getTranscoder() failed: {e}")))?;

    let last_reward_round: U256 = t.0;

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
            (Address::zero(), Address::zero())
        }
    };

    let call = with_l2_fees(
        bonding.client_ref(),
        bonding.reward_with_hint(prev, next),
        cfg,
    )
    .await?;
    let pending = call
        .send()
        .await
        .map_err(|e| AppError::Tx(format!("rewardWithHint() send failed: {e}")))?;

    let tx_hash: TxHash = *pending;
    info!("reward tx sent: tx_hash={:?}", tx_hash);

    // Wait for receipt with a timeout so we don't hang forever.
    match timeout(Duration::from_secs(receipt_timeout_secs), pending).await {
        Ok(Ok(Some(receipt))) => {
            info!(
                "reward tx confirmed: tx_hash={:?} status={:?} block={:?} gas_used={:?}",
                receipt.transaction_hash, receipt.status, receipt.block_number, receipt.gas_used
            );
            Ok(())
        }
        Ok(Ok(None)) => Err(AppError::Tx(format!(
            "reward tx pending returned None receipt: tx_hash={:?}",
            tx_hash
        ))),
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

/// Sets EIP-1559 fees from the latest base fee instead of ethers' default estimator.
///
/// ethers assumes Ethereum mainnet and uses a minimum 3 gwei tip, which on Arbitrum
/// (base fee ~0.01 gwei) inflates maxFeePerGas by 100x+. eth_estimateGas caps gas at
/// balance / maxFeePerGas, so a modestly funded wallet intermittently fails with
/// "gas required exceeds allowance". Arbitrum ignores the tip, so a zero tip is fine.
async fn with_l2_fees<M: Middleware, D>(
    client: &M,
    mut call: ContractCall<M, D>,
    cfg: &Config,
) -> Result<ContractCall<M, D>, AppError> {
    let block = client
        .get_block(BlockNumber::Latest)
        .await
        .map_err(|e| AppError::Provider(format!("get_block(latest) failed: {e}")))?
        .ok_or_else(|| AppError::Provider("latest block not found".into()))?;
    let base_fee = block
        .base_fee_per_gas
        .ok_or_else(|| AppError::Provider("latest block has no baseFeePerGas".into()))?;

    let priority_fee = cfg.max_priority_fee_wei;
    let max_fee = base_fee * U256::from(cfg.max_fee_base_multiplier) + priority_fee;

    if let TypedTransaction::Eip1559(ref mut inner) = call.tx {
        inner.max_fee_per_gas = Some(max_fee);
        inner.max_priority_fee_per_gas = Some(priority_fee);
    }

    debug!(
        "tx fees: baseFeeWei={} maxFeePerGasWei={} maxPriorityFeePerGasWei={}",
        base_fee, max_fee, priority_fee
    );

    Ok(call)
}

async fn handle_locked_round_actions<M: Middleware>(
    bonding: &BondingManager<M>,
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
            .pending_stake(orchestrator, current_round)
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
                U256::zero()
            }
        };

        if !transferable.is_zero() {
            info!(
                "transferBond sending: round={} from_orchestrator={:?} to_receiver={:?} amountWei={}",
                current_round, orchestrator, receiver, transferable
            );

            let call = with_l2_fees(
                bonding.client_ref(),
                bonding.transfer_bond(
                    receiver,
                    transferable,
                    Address::zero(),
                    Address::zero(),
                    Address::zero(),
                    Address::zero(),
                ),
                cfg,
            )
            .await?;

            match call.send().await {
                Ok(pending) => {
                    let tx_hash = *pending;
                    info!(
                        "transferBond tx sent: round={} tx_hash={:?}",
                        current_round, tx_hash
                    );

                    match timeout(Duration::from_secs(cfg.receipt_timeout_secs), pending).await {
                        Ok(Ok(Some(receipt))) => {
                            info!(
                                "transferBond confirmed: round={} tx_hash={:?} status={:?} block={:?} gas_used={:?}",
                                current_round,
                                receipt.transaction_hash,
                                receipt.status,
                                receipt.block_number,
                                receipt.gas_used
                            );
                        }
                        Ok(Ok(None)) => {
                            warn!(
                                "transferBond receipt missing (None): round={} tx_hash={:?}",
                                current_round, tx_hash
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
            .pending_fees(orchestrator, current_round)
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

            let call = with_l2_fees(
                bonding.client_ref(),
                bonding.withdraw_fees(receiver, pending_fees),
                cfg,
            )
            .await?;

            match call.send().await {
                Ok(pending) => {
                    let tx_hash = *pending;
                    info!(
                        "withdrawFees tx sent: round={} tx_hash={:?}",
                        current_round, tx_hash
                    );

                    match timeout(Duration::from_secs(cfg.receipt_timeout_secs), pending).await {
                        Ok(Ok(Some(receipt))) => {
                            info!(
                                "withdrawFees confirmed: round={} tx_hash={:?} status={:?} block={:?} gas_used={:?}",
                                current_round,
                                receipt.transaction_hash,
                                receipt.status,
                                receipt.block_number,
                                receipt.gas_used
                            );
                        }
                        Ok(Ok(None)) => {
                            warn!(
                                "withdrawFees receipt missing (None): round={} tx_hash={:?}",
                                current_round, tx_hash
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
        pending_stake: pending_stake_for_snapshot.unwrap_or_else(U256::zero),
        pending_fees: pending_fees_for_snapshot.unwrap_or_else(U256::zero),
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
