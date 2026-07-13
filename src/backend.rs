//! [`EsploraBackend`] — the public entry point: construction, mode selection,
//! and the `sync` / `rescan` / `broadcast` methods.

use bitcoin::{Network, Transaction, Txid};

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
        match self.mode {
            SyncMode::Address => crate::sync::esplora_sync(wallet, self).await,
            SyncMode::Waterfalls => crate::waterfalls::waterfalls_sync(wallet, self).await,
        }
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
