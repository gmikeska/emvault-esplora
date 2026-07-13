//! [`EsploraSyncError`] — the crate's error type.

/// Errors raised while syncing or broadcasting through the Esplora backend.
#[derive(Debug, thiserror::Error)]
pub enum EsploraSyncError {
    /// An Esplora HTTP request failed.
    #[error("esplora HTTP request failed")]
    Http(#[from] esplora_rs::Error),
    /// Esplora returned a value that didn't parse into the expected Bitcoin type.
    #[error("esplora returned a malformed {what}: {value}")]
    Malformed {
        /// What we were trying to parse (e.g. `"txid"`, `"block hash"`).
        what: &'static str,
        /// The offending raw value.
        value: String,
    },
    /// The assembled update couldn't be connected to the wallet's local chain
    /// (usually a reorg below the last persisted tip).
    #[error("failed to connect esplora update to the wallet's local chain")]
    CannotConnect(#[from] bdk_wallet::chain::local_chain::CannotConnectError),
}
