//! Contract bindings, config loading, RPC client setup and reward-hint logic shared by the
//! `funds_transfer` binary and tests.

pub mod client;
pub mod config;
pub mod contracts;
pub mod reward_hints;

#[cfg(test)]
mod mock_rpc;
