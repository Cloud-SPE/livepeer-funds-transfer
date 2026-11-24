//! Scripted JSON-RPC transport for unit tests.
//!
//! Responses are queued per method, so tests do not depend on the order in which alloy's fillers
//! issue their concurrent requests (`eth_estimateGas` and `eth_getTransactionCount` before a send).
//! Every request is recorded in arrival order, and the `expect_*` helpers assert them front to
//! back.
//!
//! This file is compiled into both the library's and the binary's test modules (via `#[path]`), so
//! each uses only part of it.
#![allow(dead_code)]

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use alloy::{
    consensus::TxEnvelope,
    contract::{CallBuilder, CallDecoder},
    eips::eip2718::Decodable2718,
    primitives::{Address, B256, Bytes},
    providers::Provider,
    rpc::{
        client::RpcClient,
        json_rpc::{
            ErrorPayload, RequestPacket, Response, ResponsePacket, ResponsePayload,
            SerializedRequest,
        },
        types::TransactionRequest,
    },
    sol_types::SolValue,
    transports::{TransportError, TransportErrorKind, TransportFut},
};
use serde::Serialize;
use serde_json::{Value, json, value::RawValue};

/// A recorded request: method and decoded params.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Debug, Default)]
pub struct MockRpc {
    responses: Arc<Mutex<HashMap<String, VecDeque<ResponsePayload>>>>,
    requests: Arc<Mutex<VecDeque<Request>>>,
}

impl MockRpc {
    pub fn new() -> Self {
        Self::default()
    }

    /// An RPC client backed by this mock (marked local, so alloy polls quickly).
    pub fn client(&self) -> RpcClient {
        RpcClient::new(self.clone(), true)
    }

    // ---------------------------------------------------------------
    // Scripting responses
    // ---------------------------------------------------------------

    pub fn push(&self, method: &str, payload: ResponsePayload) {
        self.responses
            .lock()
            .unwrap()
            .entry(method.to_string())
            .or_default()
            .push_back(payload);
    }

    /// Queues a successful `method` response serializing to `value`.
    pub fn ok<R: Serialize + ?Sized>(&self, method: &str, value: &R) {
        let raw = RawValue::from_string(serde_json::to_string(value).unwrap()).unwrap();
        self.push(method, ResponsePayload::Success(raw));
    }

    /// Queues a JSON-RPC error response with message `boom`.
    pub fn err(&self, method: &str) {
        self.push(
            method,
            ResponsePayload::Failure(ErrorPayload {
                code: -32000,
                message: "boom".into(),
                data: None,
            }),
        );
    }

    /// Queues an `eth_call` returning the ABI encoding of `value`.
    ///
    /// Static tuples and fixed arrays encode exactly like the same values returned as separate
    /// outputs, so `[U256; 10]` stands in for a function returning ten uint256s.
    pub fn ret<T: SolValue>(&self, value: T) {
        self.ok("eth_call", &Bytes::from(value.abi_encode()));
    }

    /// Queues an `eth_call` returning no data at all.
    pub fn ret_empty(&self) {
        self.ok("eth_call", &Bytes::new());
    }

    /// Queues `eth_getBlockByNumber` returning a block with `base_fee`, or no block for `None`.
    pub fn latest_block(&self, base_fee: Option<u64>) {
        self.ok("eth_getBlockByNumber", &block_json(base_fee));
    }

    pub fn no_block(&self) {
        self.ok("eth_getBlockByNumber", &Value::Null);
    }

    /// Queues the responses for one signed send: gas estimate, pending nonce, and the hash the
    /// node returns for `eth_sendRawTransaction`.
    pub fn sendable(&self, gas: u64, nonce: u64, tx_hash: B256) {
        self.ok("eth_estimateGas", &format!("{gas:#x}"));
        self.ok("eth_getTransactionCount", &format!("{nonce:#x}"));
        self.ok("eth_sendRawTransaction", &tx_hash);
    }

    /// Queues `eth_getTransactionReceipt` returning a mined receipt.
    pub fn receipt(&self, tx_hash: B256, success: bool, gas_used: u64) {
        self.ok(
            "eth_getTransactionReceipt",
            &receipt_json(tx_hash, success, gas_used),
        );
    }

    /// Queues `eth_getTransactionReceipt` returning null (not mined yet).
    pub fn no_receipt(&self) {
        self.ok("eth_getTransactionReceipt", &Value::Null);
    }

    // ---------------------------------------------------------------
    // Asserting requests
    // ---------------------------------------------------------------

    /// Removes and returns the oldest recorded request.
    pub fn next_request(&self) -> Option<Request> {
        self.requests.lock().unwrap().pop_front()
    }

    /// The method of the oldest recorded request, without removing it.
    pub fn peek_method(&self) -> Option<String> {
        self.requests
            .lock()
            .unwrap()
            .front()
            .map(|r| r.method.clone())
    }

    /// Asserts the next request is `method` and returns its params.
    #[track_caller]
    pub fn expect(&self, method: &str) -> Value {
        let req = self
            .next_request()
            .unwrap_or_else(|| panic!("expected {method}, but no more requests"));
        assert_eq!(req.method, method, "unexpected request: {req:?}");
        req.params
    }

    /// Asserts the next request is an `eth_call` of `call` (same target and calldata).
    #[track_caller]
    pub fn expect_call<P: Provider, D: CallDecoder>(&self, call: &CallBuilder<P, D>) {
        let params = self.expect("eth_call");
        let tx = params[0].clone();
        let sent: TransactionRequest = serde_json::from_value(tx.clone()).unwrap();
        let expected: &TransactionRequest = call.as_ref();
        assert_eq!(sent.to, expected.to, "eth_call target: {tx}");
        assert_eq!(
            sent.input.input(),
            Some(call.calldata()),
            "eth_call calldata: {tx}"
        );
    }

    /// Asserts the next request is `eth_getBlockByNumber("latest", false)`.
    #[track_caller]
    pub fn expect_latest_block(&self) {
        assert_eq!(
            self.expect("eth_getBlockByNumber"),
            json!(["latest", false])
        );
    }

    /// Asserts the gas estimate and nonce lookup (in either order) and then
    /// `eth_sendRawTransaction`, and returns the decoded signed tx.
    #[track_caller]
    pub fn expect_signed_send(&self) -> TxEnvelope {
        let mut prep: Vec<String> = (0..2)
            .map(|_| self.next_request().expect("send preparation").method)
            .collect();
        prep.sort();
        assert_eq!(prep, ["eth_estimateGas", "eth_getTransactionCount"]);
        let params = self.expect("eth_sendRawTransaction");
        let raw: Bytes = serde_json::from_value(params[0].clone()).unwrap();
        TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap()
    }

    /// Asserts the next request polls the receipt of `tx_hash`.
    #[track_caller]
    pub fn expect_receipt_poll(&self, tx_hash: B256) {
        assert_eq!(self.expect("eth_getTransactionReceipt"), json!([tx_hash]));
    }

    #[track_caller]
    pub fn expect_no_more_requests(&self) {
        let left: Vec<_> = self.requests.lock().unwrap().drain(..).collect();
        assert!(left.is_empty(), "unexpected requests: {left:?}");
    }

    // ---------------------------------------------------------------
    // Transport
    // ---------------------------------------------------------------

    fn respond(&self, req: SerializedRequest) -> Result<Response, TransportError> {
        let method = req.method().to_string();
        let params = req
            .params()
            .map(|p| serde_json::from_str(p.get()).unwrap())
            .unwrap_or(Value::Null);
        self.requests.lock().unwrap().push_back(Request {
            method: method.clone(),
            params,
        });
        let payload = self
            .responses
            .lock()
            .unwrap()
            .get_mut(&method)
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| {
                TransportErrorKind::custom_str(&format!("no scripted response for {method}"))
            })?;
        Ok(Response {
            id: req.id().clone(),
            payload,
        })
    }
}

impl tower::Service<RequestPacket> for MockRpc {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            Ok(match req {
                RequestPacket::Single(req) => ResponsePacket::Single(this.respond(req)?),
                RequestPacket::Batch(reqs) => ResponsePacket::Batch(
                    reqs.into_iter()
                        .map(|r| this.respond(r))
                        .collect::<Result<_, _>>()?,
                ),
            })
        })
    }
}

/// A minimal `eth_getBlockByNumber` result.
fn block_json(base_fee: Option<u64>) -> Value {
    let zero32 = B256::ZERO;
    let mut block = json!({
        "hash": B256::with_last_byte(0x0b),
        "parentHash": zero32,
        "sha3Uncles": zero32,
        "miner": Address::ZERO,
        "stateRoot": zero32,
        "transactionsRoot": zero32,
        "receiptsRoot": zero32,
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "difficulty": "0x1",
        "number": "0x64",
        "gasLimit": "0x4000000000000",
        "gasUsed": "0x0",
        "timestamp": "0x6553f100",
        "extraData": "0x",
        "mixHash": zero32,
        "nonce": "0x0000000000000000",
        "uncles": [],
        "transactions": [],
    });
    if let Some(fee) = base_fee {
        block["baseFeePerGas"] = json!(format!("{fee:#x}"));
    }
    block
}

/// A minimal EIP-1559 `eth_getTransactionReceipt` result.
fn receipt_json(tx_hash: B256, success: bool, gas_used: u64) -> Value {
    json!({
        "type": "0x2",
        "status": if success { "0x1" } else { "0x0" },
        "cumulativeGasUsed": format!("{gas_used:#x}"),
        "logs": [],
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "transactionHash": tx_hash,
        "transactionIndex": "0x0",
        "blockHash": B256::with_last_byte(0x0b),
        "blockNumber": "0x65",
        "gasUsed": format!("{gas_used:#x}"),
        "effectiveGasPrice": "0x989680",
        "from": Address::ZERO,
        "to": Address::ZERO,
        "contractAddress": null,
    })
}
