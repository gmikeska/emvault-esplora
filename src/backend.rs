//! [`EsploraBackend`] — the public entry point: construction, mode selection,
//! and the `sync` / `rescan` / `broadcast` methods.

use std::collections::{BTreeMap, HashSet};

use bitcoin::{BlockHash, Network, Transaction, Txid};

use bdk_wallet::KeychainKind;
use bdk_wallet::chain::ChainPosition;

use crate::convert;
use crate::error::EsploraSyncError;
use crate::result::EsploraSyncResult;

/// Which scan strategy [`EsploraBackend::sync`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Address-based gap / revealed-range scan (works on any Esplora).
    #[default]
    Address,
    /// `QuickSync` / Waterfalls descriptor scan (enterprise; one query per keychain).
    Waterfalls,
}

/// Sync tuning knobs. `Default` = gap limit 20, sequential.
#[derive(Debug, Clone, Copy)]
pub struct EsploraSyncOpts {
    /// Stop scanning a keychain after this many consecutive unused addresses.
    pub gap_limit: u32,
    /// Reserved for future concurrent SPK fetching. `1` = sequential (current).
    pub parallelism: usize,
}

impl Default for EsploraSyncOpts {
    fn default() -> Self {
        Self {
            gap_limit: 20,
            parallelism: 1,
        }
    }
}

/// A nodeless Esplora chain backend: an [`esplora_rs::Client`] plus the target
/// network, sync options, and [`SyncMode`].
#[derive(Debug, Clone)]
pub struct EsploraBackend {
    client: esplora_rs::Client,
    network: Network,
    opts: EsploraSyncOpts,
    mode: SyncMode,
}

impl EsploraBackend {
    /// Unauthenticated public / self-hosted Esplora.
    ///
    /// # Errors
    /// Returns [`EsploraSyncError::Http`] if the base URL is invalid.
    pub fn new_public(base_url: &str, network: Network) -> Result<Self, EsploraSyncError> {
        Ok(Self {
            client: esplora_rs::Client::new_public(&ensure_trailing_slash(base_url))?,
            network,
            opts: EsploraSyncOpts::default(),
            mode: SyncMode::default(),
        })
    }

    /// Enterprise Esplora (OAuth Bearer). Credentials are read by `esplora-rs`
    /// from `ESPLORA_CLIENT_ID` / `ESPLORA_CLIENT_SECRET`.
    ///
    /// # Errors
    /// Returns [`EsploraSyncError::Http`] if the base URL is invalid.
    pub fn new_enterprise(base_url: &str, network: Network) -> Result<Self, EsploraSyncError> {
        Ok(Self {
            client: esplora_rs::Client::new(&ensure_trailing_slash(base_url))?,
            network,
            opts: EsploraSyncOpts::default(),
            mode: SyncMode::default(),
        })
    }

    /// Auto-detecting constructor (reach for this one): **enterprise** iff both
    /// `ESPLORA_CLIENT_ID` and `ESPLORA_CLIENT_SECRET` are set and non-empty,
    /// otherwise **public**. Folds the branch that used to live in the app.
    ///
    /// # Errors
    /// Returns [`EsploraSyncError::Http`] if the base URL is invalid.
    pub fn connect(base_url: &str, network: Network) -> Result<Self, EsploraSyncError> {
        if cred_present("ESPLORA_CLIENT_ID") && cred_present("ESPLORA_CLIENT_SECRET") {
            Self::new_enterprise(base_url, network)
        } else {
            Self::new_public(base_url, network)
        }
    }

    /// Set the [`SyncMode`] (default [`SyncMode::Address`]).
    #[must_use]
    pub fn with_mode(mut self, mode: SyncMode) -> Self {
        self.mode = mode;
        self
    }

    /// Override the sync options.
    #[must_use]
    pub fn with_opts(mut self, opts: EsploraSyncOpts) -> Self {
        self.opts = opts;
        self
    }

    /// The network derived keys are stamped with.
    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }

    /// The configured sync mode.
    #[must_use]
    pub fn mode(&self) -> SyncMode {
        self.mode
    }

    /// The underlying `esplora-rs` client.
    #[must_use]
    pub fn client(&self) -> &esplora_rs::Client {
        &self.client
    }

    pub(crate) fn gap_limit(&self) -> u32 {
        self.opts.gap_limit
    }

    /// Sync `wallet` using the configured [`SyncMode`]. In `Address` mode the
    /// first call on a fresh wallet full-scans and steady state is incremental;
    /// `Waterfalls` mode always does a full descriptor scan.
    ///
    /// # Errors
    /// Surfaces HTTP, parse, and chain-connection failures via [`EsploraSyncError`].
    pub async fn sync(
        &self,
        wallet: &mut bdk_wallet::Wallet,
    ) -> Result<EsploraSyncResult, EsploraSyncError> {
        // Snapshot pre-sync state *before* the scan's `apply_update` mutates the
        // graph — the confirmed-txid baseline for the eviction diff and the
        // local-chain checkpoint hashes (the reorg signal). Mirrors
        // `emvault-electrum`'s `ElectrumBackend::sync`.
        let before_confirmed = confirmed_txids(wallet);
        let checkpoints_pre = checkpoint_hashes(wallet);
        let h_pre = wallet.latest_checkpoint().height();

        let result = match self.mode {
            SyncMode::Address => crate::sync::esplora_sync(wallet, self).await?,
            SyncMode::Waterfalls => crate::waterfalls::waterfalls_sync(wallet, self).await?,
        };

        // A reorg below the persisted tip reorgs the local chain cleanly but leaves
        // a phantom UTXO that only a from-scratch rebuild clears (proven for the
        // electrum backend in `emvault-electrum/tests/live_reorg.rs`; the merge-only
        // BDK update model is identical here). Detection reconciles the wallet's
        // stored checkpoint hashes against server ground truth: the scan's
        // `apply_update` only *inserts* anchors + tip (see
        // `internal::extend_checkpoint_to_tip`) and never re-checks an existing
        // checkpoint height, so a silent reorg leaves the stale hash in place and a
        // wallet-only pre/post compare misses it. A fresh wallet (`h_pre == 0`)
        // can't reorg — it just did a full scan — so only guard the steady-state path.
        if h_pre > 0 && self.server_reorg_below_tip(&checkpoints_pre, h_pre).await? {
            return self.rebuild_on_reorg(wallet, &before_confirmed).await;
        }

        Ok(result)
    }

    /// Reorg-recovery rebuild (decisions D2/D3/D5), the esplora mirror of
    /// `emvault-electrum`'s. Reconstructs the wallet's tx graph from scratch via a
    /// full descriptor scan — the only operation that clears the reorged-out phantom
    /// UTXO — and **replaces** the caller's wallet in place (D2). `evicted_txids` =
    /// confirmed-before minus present-anywhere in the rebuilt graph (D5: absent
    /// *entirely*, so a re-queued mempool tx is not evicted).
    ///
    /// The returned `changeset` is the **complete** rebuilt changeset with
    /// `reorg_rebuilt: true`; the app must persist it as a **replace**, not a merge.
    async fn rebuild_on_reorg(
        &self,
        wallet: &mut bdk_wallet::Wallet,
        before_confirmed: &HashSet<Txid>,
    ) -> Result<EsploraSyncResult, EsploraSyncError> {
        // Rebuild from the live wallet's own public (watch-only) descriptors — a
        // scan-equivalent wallet. Signing keys are intentionally not carried; the
        // app re-registers signers after a rebuild (it does so on every load).
        let ext = wallet.public_descriptor(KeychainKind::External).clone();
        let int = wallet.public_descriptor(KeychainKind::Internal).clone();
        let reveal_ext = wallet.derivation_index(KeychainKind::External);
        let reveal_int = wallet.derivation_index(KeychainKind::Internal);

        let mut fresh = bdk_wallet::Wallet::create(ext, int)
            .network(self.network)
            .create_wallet_no_persist()
            .map_err(|e| EsploraSyncError::Rebuild(e.to_string()))?;

        // Preserve address continuity: reveal the rebuilt wallet up to the indices
        // the live wallet had handed out *before* the scan, so the single
        // `take_staged` inside the rescan folds the reveals into the one complete
        // replacement changeset (and we never re-issue a shown address).
        if let Some(idx) = reveal_ext {
            let _ = fresh
                .reveal_addresses_to(KeychainKind::External, idx)
                .count();
        }
        if let Some(idx) = reveal_int {
            let _ = fresh
                .reveal_addresses_to(KeychainKind::Internal, idx)
                .count();
        }

        // Full descriptor scan (D3) self-discovers used addresses. Use the same
        // strategy as the configured mode so enterprise/public parity holds.
        let rebuilt = match self.mode {
            SyncMode::Address => crate::sync::esplora_rescan(&mut fresh, self).await?,
            SyncMode::Waterfalls => crate::waterfalls::waterfalls_sync(&mut fresh, self).await?,
        };

        // evicted = confirmed-before − present-anywhere-after (D5).
        let after_all: HashSet<Txid> = fresh.transactions().map(|wtx| wtx.tx_node.txid).collect();
        let mut evicted_txids: Vec<Txid> = before_confirmed
            .iter()
            .filter(|txid| !after_all.contains(*txid))
            .copied()
            .collect();
        evicted_txids.sort_unstable(); // deterministic order for the app/audit trail

        // Swap in the rebuilt wallet (D2: the backend replaces the wallet).
        // `rebuilt.changeset` is the complete fresh changeset (descriptor + reveals
        // + full graph) — the app persists it as a replace, keyed off `reorg_rebuilt`.
        let tip_height = fresh.latest_checkpoint().height();
        *wallet = fresh;

        Ok(EsploraSyncResult {
            changeset: rebuilt.changeset,
            blocks_synced: 0,
            new_mempool_txs: 0,
            tip_height,
            evicted_txids,
            reorg_rebuilt: true,
        })
    }

    /// Authoritative reorg-below-tip detection: reconcile the wallet's stored
    /// checkpoint hashes against the server's actual block hash at each height.
    /// Returns `true` iff any stored checkpoint at height `<= h_pre` (genesis
    /// excepted) now resolves to a **different** hash on the server.
    ///
    /// This replaces a wallet-only pre/post checkpoint compare, which is blind to a
    /// "silent" reorg: the scan's `apply_update` only inserts anchor blocks + the
    /// fresh tip (via `internal::extend_checkpoint_to_tip`) and never re-fetches the
    /// hash at an existing checkpoint height, so an orphaned checkpoint keeps its
    /// stale hash on both sides and the reorg is never seen. Re-fetching ground
    /// truth is the same guarantee `bdk_electrum` gets natively — it re-derives
    /// checkpoints from the server each sync.
    ///
    /// All stored checkpoints `<= h_pre` are checked — no depth ceiling: an
    /// unbounded guarantee beats capping round-trips, since a reorg deeper than any
    /// cap would silently leave the phantom UTXO (the exact failure this fixes).
    /// Checkpoints are sparse (anchored heights + tip), so this is a handful of
    /// requests per sync.
    ///
    /// # Errors
    /// Surfaces HTTP / parse failures via [`EsploraSyncError`].
    async fn server_reorg_below_tip(
        &self,
        checkpoints_pre: &BTreeMap<u32, BlockHash>,
        h_pre: u32,
    ) -> Result<bool, EsploraSyncError> {
        let server_tip = u32::try_from(self.client.get_tip_height().await?).unwrap_or(u32::MAX);
        for (&height, &stored_hash) in checkpoints_pre.range(..=h_pre) {
            // Genesis never reorgs; a checkpoint above the server's current tip
            // can't be reconciled (a shorter/behind chain is not a below-tip reorg)
            // — skip both without a round-trip.
            if height == 0 || height > server_tip {
                continue;
            }
            let server_hash = convert::block_hash(
                &self
                    .client
                    .get_block_hash_from_height(u64::from(height))
                    .await?,
            )?;
            if server_hash != stored_hash {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Force a full rescan regardless of steady state (for a "Rescan" action).
    /// `Address` mode does a full gap scan; `Waterfalls` mode is already a full
    /// descriptor scan.
    ///
    /// # Errors
    /// Surfaces HTTP, parse, and chain-connection failures via [`EsploraSyncError`].
    pub async fn rescan(
        &self,
        wallet: &mut bdk_wallet::Wallet,
    ) -> Result<EsploraSyncResult, EsploraSyncError> {
        match self.mode {
            SyncMode::Address => crate::sync::esplora_rescan(wallet, self).await,
            SyncMode::Waterfalls => crate::waterfalls::waterfalls_sync(wallet, self).await,
        }
    }

    /// Broadcast a fully-signed transaction; returns its txid.
    ///
    /// # Errors
    /// Returns [`EsploraSyncError::Http`] if the broadcast is rejected, or
    /// [`EsploraSyncError::Malformed`] if the returned txid doesn't parse.
    pub async fn broadcast(&self, tx: &Transaction) -> Result<Txid, EsploraSyncError> {
        let hex = bitcoin::consensus::encode::serialize_hex(tx);
        let txid = self.client.broadcast_tx(&hex).await?;
        crate::convert::txid(&txid)
    }
}

/// `true` iff `name` is present in the environment and non-empty after trimming.
fn cred_present(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.trim().is_empty())
}

/// The set of txids the wallet's graph currently considers **confirmed**. Snapshot
/// before a scan's `apply_update` to form the eviction baseline (a reorg-below-tip
/// demotes the reorged-out tx to unconfirmed during `apply_update`, so a post-scan
/// read would miss it).
fn confirmed_txids(wallet: &bdk_wallet::Wallet) -> HashSet<Txid> {
    wallet
        .transactions()
        .filter(|wtx| matches!(wtx.chain_position, ChainPosition::Confirmed { .. }))
        .map(|wtx| wtx.tx_node.txid)
        .collect()
}

/// Snapshot the wallet's local-chain checkpoints as `height -> block hash`. The
/// set is sparse (anchored heights plus the tip), so this is cheap.
fn checkpoint_hashes(wallet: &bdk_wallet::Wallet) -> BTreeMap<u32, BlockHash> {
    wallet
        .local_chain()
        .tip()
        .iter()
        .map(|cp| (cp.height(), cp.hash()))
        .collect()
}

/// `Url::join` drops the last segment when the base lacks a trailing slash
/// (`…/api` + `blocks/tip` → `…/blocks/tip`). Normalize once at construction.
fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_owned()
    } else {
        format!("{url}/")
    }
}

#[cfg(test)]
mod tests {
    use super::{EsploraBackend, EsploraSyncOpts, SyncMode, cred_present, ensure_trailing_slash};
    use bitcoin::Network;

    #[test]
    fn trailing_slash_added_once() {
        assert_eq!(ensure_trailing_slash("https://x/api"), "https://x/api/");
        assert_eq!(ensure_trailing_slash("https://x/api/"), "https://x/api/");
    }

    #[test]
    fn cred_present_treats_empty_and_whitespace_as_absent() {
        let key = "EMVAULT_ESPLORA_CRED_PRESENT_UNIT_TEST";
        // SAFETY: single-threaded within this test; a crate-unique var name so it
        // can't collide with other tests reading real ESPLORA_* vars.
        unsafe { std::env::remove_var(key) };
        assert!(!cred_present(key), "unset → absent");
        unsafe { std::env::set_var(key, "   ") };
        assert!(!cred_present(key), "whitespace-only → absent");
        unsafe { std::env::set_var(key, "id") };
        assert!(cred_present(key), "non-empty → present");
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn sync_mode_default_is_address() {
        assert_eq!(SyncMode::default(), SyncMode::Address);
    }

    #[test]
    fn opts_default_is_gap_20_sequential() {
        let o = EsploraSyncOpts::default();
        assert_eq!(o.gap_limit, 20);
        assert_eq!(o.parallelism, 1);
    }

    #[test]
    fn builder_carries_mode_opts_and_network() {
        let backend = EsploraBackend::new_public("https://x/api", Network::Testnet)
            .expect("backend")
            .with_mode(SyncMode::Waterfalls)
            .with_opts(EsploraSyncOpts {
                gap_limit: 5,
                parallelism: 1,
            });
        assert_eq!(backend.mode(), SyncMode::Waterfalls);
        assert_eq!(backend.gap_limit(), 5);
        assert_eq!(backend.network(), Network::Testnet);
    }
}
