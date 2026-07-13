//! Nodeless **Esplora + Waterfalls** chain backend for [EmVault](https://github.com/gmikeska)
//! (BDK) wallets.
//!
//! This crate lets a `bdk_wallet::Wallet` sync and broadcast against a
//! Blockstream-style [Esplora](https://github.com/Blockstream/esplora) HTTP API —
//! no Bitcoin Core node required. It is meant to be enabled behind
//! `emvault-core`'s `esplora` feature (which re-exports it as
//! `emvault_core::esplora`), but it depends only on `bdk_wallet` / `bitcoin` /
//! [`esplora-rs`], so it can be used directly too.
//!
//! # One backend, two strategies
//!
//! A single [`EsploraBackend`] carries a [`SyncMode`]:
//!
//! - [`SyncMode::Address`] — gap-limited address scan; works on **any** Esplora
//!   (public or enterprise).
//! - [`SyncMode::Waterfalls`] — one `QuickSync` / Waterfalls descriptor query per
//!   keychain (enterprise tier). Far fewer requests, but the server sees the
//!   descriptor → suitable for dev/staging, not privacy-sensitive production.
//!
//! ```no_run
//! use emvault_esplora::{EsploraBackend, SyncMode};
//! use bdk_wallet::bitcoin::Network;
//! # async fn demo(wallet: &mut bdk_wallet::Wallet, signed: &bdk_wallet::bitcoin::Transaction) -> Result<(), emvault_esplora::EsploraSyncError> {
//! let backend = EsploraBackend::connect("https://enterprise.blockstream.info/testnet/api", Network::Testnet)?
//!     .with_mode(SyncMode::Waterfalls);          // pick the strategy once
//! let result = backend.sync(wallet).await?;       // mode-agnostic
//! let txid   = backend.broadcast(signed).await?;
//! # let _ = result; let _ = txid; Ok(())
//! # }
//! ```
//!
//! # Credentials (env)
//!
//! [`EsploraBackend::connect`] auto-selects **enterprise** (OAuth Bearer) when
//! both `ESPLORA_CLIENT_ID` and `ESPLORA_CLIENT_SECRET` are set and non-empty,
//! else **public**. Those two vars are read by [`esplora-rs`] itself; the base
//! URL is always injected as a parameter. Use [`EsploraBackend::new_public`] /
//! [`EsploraBackend::new_enterprise`] to force one explicitly.
//!
//! # Invariants (enforced here so callers can't regress them)
//!
//! Every sync path is **sequential** (no `buffer_unordered`) and holds no
//! borrowed value across an `.await`, so the returned futures are `for<'a> Send`
//! and safe to call from inside `axum` request handlers. The `bitcoin`-typed
//! boundary lives entirely in the private `convert` module. See
//! `docs/design/salvage-from-emvault-core.md` §1 for the full rationale.
//!
//! [`esplora-rs`]: esplora_rs

mod backend;
mod convert;
mod error;
mod internal;
mod sync;
mod waterfalls;

pub use backend::{EsploraBackend, EsploraSyncOpts, SyncMode};
pub use error::EsploraSyncError;
pub use result::EsploraSyncResult;

mod result;
