# emvault-esplora — integration design (core + pkcs11), ergonomics-first

Companion to [`salvage-from-emvault-core.md`](salvage-from-emvault-core.md)
(which holds the port-ready code blocks). This doc is the **integration
surface**: how `emvault-esplora` is shaped, how `emvault-core` picks it up behind
the `esplora` feature, and how `test-app-pkcs11` consumes it — optimized so
adoption is as close to a one-liner as the node path (`emitter_sync`).

Decisions (§6) are settled — Greg approved all five recommendations 2026-07-13.

## Design principles (ergonomics)
1. **One import, one constructor, one `.sync()`.** A consumer should not have to
   hand-branch on public/enterprise, or remember which of three sync functions
   to call.
2. **Same seam as the node backend.** Esplora sync returns the same result shape
   consumers already use for `emitter_sync`, so swapping backends is a config
   change, not a code change.
3. **Hard to misuse.** The `for<'a> Send`, `FullScanResponse`-vs-`SyncResponse`,
   and bitcoin-free-boundary invariants (salvage §1) are encapsulated — callers
   can't reintroduce them.
4. **Feature-gated & optional.** Zero cost / zero deps when `esplora` is off.

---

## 1. `emvault-esplora` crate layout

Currently a **bin** scaffold (`src/main.rs`) — make it a **library** (`src/lib.rs`).

```
src/
  lib.rs        // façade: pub use of the surface below + crate `//!` docs
  error.rs      // EsploraSyncError                       (salvage §2)
  convert.rs    // private String→bitcoin boundary + tests (salvage §3)
  result.rs     // EsploraSyncResult                      (salvage §9)
  backend.rs    // EsploraBackend, EsploraSyncOpts, SyncMode, ensure_trailing_slash
  sync.rs       // address path: rescan + incremental + ingest_tx/address helpers (salvage §4–6)
  waterfalls.rs // waterfalls_sync                        (salvage §7)
docs/design/…   // this doc + salvage + reference/ tests
```
Deps: `bdk_wallet` 3.1, `bitcoin` 0.32, `esplora-rs`, `thiserror` 2, `hex`.
Dev: `tokio`, `serde_json`, `hex`, `dotenvy`. **No `futures`** (invariant §1.1).

---

## 2. Public API — the ergonomic surface

```rust
/// Which scan strategy the backend uses for `sync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Address-based gap / revealed-range scan (works on any Esplora).
    #[default]
    Address,
    /// QuickSync / Waterfalls descriptor scan (enterprise; one query per keychain).
    Waterfalls,
}

pub struct EsploraBackend { /* client, network, opts, mode */ }

impl EsploraBackend {
    /// Unauthenticated public / self-hosted Esplora.
    pub fn new_public(base_url: &str, network: Network) -> Result<Self, EsploraSyncError>;
    /// Enterprise Esplora (OAuth Bearer; creds from env via esplora-rs).
    pub fn new_enterprise(base_url: &str, network: Network) -> Result<Self, EsploraSyncError>;
    /// **Auto-detect** (the one to reach for): enterprise iff `ESPLORA_CLIENT_ID`
    /// + `ESPLORA_CLIENT_SECRET` are both set, else public. Folds the branch that
    /// used to live in the app.
    pub fn connect(base_url: &str, network: Network) -> Result<Self, EsploraSyncError>;

    #[must_use] pub fn with_mode(self, mode: SyncMode) -> Self;   // default Address
    #[must_use] pub fn with_opts(self, opts: EsploraSyncOpts) -> Self;
    #[must_use] pub fn network(&self) -> Network;
    #[must_use] pub fn client(&self) -> &esplora_rs::Client;

    /// Sync `wallet` using the configured `SyncMode`. First call on a fresh
    /// wallet full-scans; steady state is incremental (Address mode).
    pub async fn sync(&self, wallet: &mut Wallet) -> Result<EsploraSyncResult, EsploraSyncError>;
    /// Force a full descriptor/gap rescan regardless of mode (for a "Rescan" button).
    pub async fn rescan(&self, wallet: &mut Wallet) -> Result<EsploraSyncResult, EsploraSyncError>;
    /// Broadcast a signed tx; returns its txid.
    pub async fn broadcast(&self, tx: &Transaction) -> Result<Txid, EsploraSyncError>;
}
```

**Adoption (the whole point):**
```rust
use emvault_core::esplora::{EsploraBackend, SyncMode};      // via core's re-export (§3)

let backend = EsploraBackend::connect(&url, network)?       // auto public/enterprise
    .with_mode(SyncMode::Waterfalls);                       // pick strategy once
let result = backend.sync(&mut wallet).await?;              // mode-agnostic call
let txid   = backend.broadcast(&signed_tx).await?;
```

Internally `sync()` dispatches: `Address` → `esplora_sync` (rescan first,
incremental after); `Waterfalls` → `esplora_waterfalls_sync`. The three
free functions from core (salvage §6/§7) become **private** impl detail; the
backend methods are the only public entry points → one obvious way to do it.

---

## 3. `emvault-core` integration (feature `esplora`)

```toml
# emvault-core/Cargo.toml
[features]
esplora = ["dep:emvault-esplora"]

[dependencies]
emvault-esplora = { path = "../emvault-esplora", optional = true }
```
```rust
// emvault-core/src/lib.rs
#[cfg(feature = "esplora")]
pub use emvault_esplora as esplora;   // consumers: emvault_core::esplora::{EsploraBackend, SyncMode, …}

// Keep the SyncResult seam: esplora results drop into the node-path result type.
#[cfg(feature = "esplora")]
impl From<emvault_esplora::EsploraSyncResult> for chain_sync::SyncResult {
    fn from(r: emvault_esplora::EsploraSyncResult) -> Self {
        Self { changeset: r.changeset, blocks_synced: r.blocks_synced,
               new_mempool_txs: r.new_mempool_txs, tip_height: r.tip_height }
    }
}
```
That `From` is the whole reason to keep the fields identical (salvage §0/§9):
one seam for both backends, so `sync_esplora` and the node `sync` produce the
same `SyncResult`. Core gains **no** esplora code — just a re-export + a 4-field
`From`.

---

## 4. `test-app-pkcs11` integration

- **Cargo:** enable the feature — `emvault = { path = "…", features = ["esplora"] }`
  (so `emvault::core::esplora` resolves). No direct `esplora-rs` dep in the app.
- **`config.rs`:** keep `ChainBackend { Rpc, Esplora, Waterfalls }` + parsing
  (unchanged from what we built). Map at the wiring layer:
  `Esplora → SyncMode::Address`, `Waterfalls → SyncMode::Waterfalls`.
- **`wallet.rs` `WalletManager::new`:** replace the hand-branched
  public/enterprise construction + `waterfalls: bool` flag with:
  ```rust
  let esplora = match config.chain_backend {
      ChainBackend::Esplora | ChainBackend::Waterfalls => {
          let mode = if matches!(config.chain_backend, ChainBackend::Waterfalls)
              { SyncMode::Waterfalls } else { SyncMode::Address };
          Some(Arc::new(
              EsploraBackend::connect(config.esplora_url.as_deref().unwrap_or_default(), config.network)?
                  .with_mode(mode),
          ))
      }
      ChainBackend::Rpc => None,
  };
  ```
  → the `waterfalls: bool` on `WalletManager`/`UserWallet` **goes away** (mode
  lives in the backend). Net simplification vs what we had.
- **`UserWallet::sync` → `sync_esplora`:** `backend.sync(&mut wallet).await` for
  the primary + version wallets; map result (`EsploraSyncResult` or via
  `SyncResult` `From`) into `SyncSummary`. The Send-safe sequential behavior is
  now guaranteed *by the crate* — the app can't reintroduce `buffer_unordered`.
- **`broadcast_signed`:** `backend.broadcast(&tx).await`.

Everything else (the render-path `uw.sync()` calls, the `SyncSummary` mapping)
is unchanged. `.env` already has testnet + creds, so no config change.

---

## 5. Invariants (do not regress — see salvage §1)
`for<'a> Send` (sequential, owned captures, **build the consumer app** to catch
it); bitcoin-free boundary; `FullScanResponse` (first scan + waterfalls) vs
`SyncResponse` (incremental); `ensure_trailing_slash`; prevouts-are-free. These
are now enforced by construction inside the crate.

---

## 6. Decisions (settled 2026-07-13 — Greg approved all recs)

1. **API shape:** **methods on `EsploraBackend`** (`backend.sync(&mut wallet)`).
   The address/waterfalls free functions are private impl detail.
2. **Mode:** **carried in the backend** — `.with_mode(SyncMode)` once, then a
   mode-agnostic `sync()`. The app maps `APP_CHAIN_BACKEND` → mode at construction.
3. **Core coupling:** **core re-exports the crate as `emvault_core::esplora`** and
   provides `impl From<EsploraSyncResult> for chain_sync::SyncResult` (one seam
   with `emitter_sync`; no esplora code in core).
4. **Auto-detect constructor:** **`EsploraBackend::connect(url, network)`** folds
   "both creds present *and non-empty* → enterprise, else public". Name = `connect`.
5. **`esplora-rs` dep:** **`path`/`[patch]` until published** (unchanged).
