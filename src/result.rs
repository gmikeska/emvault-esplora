//! [`EsploraSyncResult`] — the staged outcome of a sync.

use bdk_wallet::ChangeSet;
use bitcoin::Txid;

/// The staged changeset and counters produced by a sync.
///
/// The fields mirror `emvault-core`'s `chain_sync::SyncResult` one-for-one so
/// core can `From`-convert this into that type (the single seam shared with the
/// Bitcoin Core `emitter_sync` backend). `changeset` is an `Option` because
/// `Wallet::take_staged` returns `None` when a sync produced no changes.
#[derive(Debug)]
pub struct EsploraSyncResult {
    /// Staged wallet changeset to persist, or `None` if nothing changed.
    pub changeset: Option<ChangeSet>,
    /// Number of blocks connected by this sync. Always `0` for Esplora (the
    /// address/waterfalls scans work per-tx, not per-block); present to keep the
    /// shape identical to the node backend.
    pub blocks_synced: u32,
    /// Count of transactions newly seen in the mempool during this sync.
    pub new_mempool_txs: u32,
    /// Height of the wallet's local chain tip after applying the update.
    pub tip_height: u32,
    /// Txids that were **confirmed** in the wallet's graph before this sync but
    /// are **absent entirely** from the chain after it (reorged/evicted out — not
    /// merely demoted to mempool, so a re-queued sweep is *not* listed here). Empty
    /// on a normal forward sync; only populated on the reorg-rebuild path
    /// ([`crate::EsploraBackend::sync`]). Mirrors `emvault-electrum`'s
    /// `ElectrumSyncResult::evicted_txids` one-for-one for the shared
    /// `emvault-core` `chain_sync::SyncResult` seam.
    pub evicted_txids: Vec<Txid>,
    /// `true` when this sync detected a reorg below the persisted tip and rebuilt
    /// the wallet's tx graph from scratch (the phantom-UTXO fix). A plain forward
    /// sync leaves it `false`. When `true`, `changeset` is the **complete** rebuilt
    /// changeset and the app must **replace** its persisted aggregate, not merge it
    /// (a merge would re-introduce the reorged-out phantom UTXO).
    pub reorg_rebuilt: bool,
}
