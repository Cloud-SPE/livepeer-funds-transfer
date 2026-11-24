//! RPC client setup and receipt polling, shared by the binary and its tests.

use std::time::Duration;

use alloy::{
    primitives::TxHash,
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::{client::RpcClient, types::TransactionReceipt},
    signers::local::PrivateKeySigner,
    transports::TransportResult,
};

/// Builds the signing provider used for every call and transaction.
///
/// It uses an explicit filler set instead of alloy's recommended one:
/// - gas: estimates the gas limit only. Callers always set both EIP-1559 fees (see
///   `with_l2_fees` in the binary), so alloy's fee estimator never runs.
/// - nonce: `eth_getTransactionCount(pending)` before every tx. The cached nonce manager keeps
///   counting locally, so a tx that never reaches the node leaves a nonce gap that stalls every
///   later tx until a restart.
/// - chain id: fixed from config, so no `eth_chainId` round trip.
/// - wallet: signs locally and sends `eth_sendRawTransaction`.
pub fn signing_provider(client: RpcClient, signer: PrivateKeySigner, chain_id: u64) -> DynProvider {
    ProviderBuilder::new()
        .disable_recommended_fillers()
        .with_gas_estimation()
        .with_simple_nonce_management()
        .with_chain_id(chain_id)
        .wallet(signer)
        .connect_client(client)
        .erased()
}

/// Polls `eth_getTransactionReceipt` every `interval` until the receipt exists.
///
/// It never gives up on its own; wrap it in `tokio::time::timeout`. alloy's
/// `PendingTransactionBuilder::get_receipt` also fetches every new block while it waits, which on
/// Arbitrum (several blocks per second) costs far more RPC calls than polling one receipt.
pub async fn wait_for_receipt<P: Provider>(
    provider: &P,
    tx_hash: TxHash,
    interval: Duration,
) -> TransportResult<TransactionReceipt> {
    loop {
        if let Some(receipt) = provider.get_transaction_receipt(tx_hash).await? {
            return Ok(receipt);
        }
        tokio::time::sleep(interval).await;
    }
}
