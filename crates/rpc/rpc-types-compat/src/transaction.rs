//! Compatibility functions for rpc `Transaction` type.

use alloy_consensus::transaction::Recovered;
use alloy_rpc_types_eth::{request::TransactionRequest, TransactionInfo};
use core::error;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Builds RPC transaction w.r.t. network.
pub trait TransactionCompat<T>: Send + Sync + Unpin + Clone + fmt::Debug {
    /// RPC transaction response type.
    type Transaction: Serialize
        + for<'de> Deserialize<'de>
        + Send
        + Sync
        + Unpin
        + Clone
        + fmt::Debug;

    /// RPC transaction error type.
    type Error: error::Error + Into<jsonrpsee_types::ErrorObject<'static>>;

    /// Wrapper for `fill()` with default `TransactionInfo`
    /// Create a new rpc transaction result for a _pending_ signed transaction, setting block
    /// environment related fields to `None`.
    fn fill_pending(&self, tx: Recovered<T>) -> Result<Self::Transaction, Self::Error> {
        self.fill(tx, TransactionInfo::default())
    }

    /// Create a new rpc transaction result for a mined transaction, using the given block hash,
    /// number, and tx index fields to populate the corresponding fields in the rpc result.
    ///
    /// The block hash, number, and tx index fields should be from the original block where the
    /// transaction was mined.
    fn fill(
        &self,
        tx: Recovered<T>,
        tx_inf: TransactionInfo,
    ) -> Result<Self::Transaction, Self::Error>;

    /// Builds a fake transaction from a transaction request for inclusion into block built in
    /// `eth_simulateV1`.
    fn build_simulate_v1_transaction(&self, request: TransactionRequest) -> Result<T, Self::Error>;
}
