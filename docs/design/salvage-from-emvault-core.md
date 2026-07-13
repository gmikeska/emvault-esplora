# Salvage from emvault-core — chain-sync blocks to port into `emvault-esplora`

Captured 2026-07-13, before reverting `emvault-core` to `master`. Source:
`emvault-core/src/esplora_sync.rs` (feature `esplora`). These are the
**high-quality, battle-tested** blocks that should be reimplemented here to
support **both** use cases: raw address-based Esplora **and** Waterfalls
descriptor scan. Everything below compiled, passed pedantic clippy, and was
verified live on testnet (waterfalls saw a real 100k-sat deposit in one query).

Only the genuinely reusable pieces are here. Trivial glue is omitted.

---

## 0. Architectural note — the one thing that MUST change (dependency inversion)

In core these functions returned `crate::chain_sync::SyncResult` (defined in
emvault-core) so the Esplora backend was a drop-in sibling of `emitter_sync`.

**That won't work once this is a crate that core depends on** — `emvault-esplora`
can't import `emvault_core::chain_sync::SyncResult` (would be a dependency
cycle). So invert it:

- **Define the result type *here*** (or return raw BDK pieces and let core adapt).
  Recommended: a local `EsploraSyncResult { changeset: ChangeSet, blocks_synced:
  u32, new_mempool_txs: u32, tip_height: u32 }`, and have emvault-core's feature
  shim `From`-convert it into core's `SyncResult`. Field-for-field identical, so
  the `From` is trivial.
- The BDK types (`Wallet`, `ChangeSet`, `TxUpdate`, `FullScanResponse`,
  `ConfirmationBlockTime`, `BlockId`, `KeychainKind`) and `bitcoin` come from
  `bdk_wallet` / `bitcoin` directly here — no core needed.

Everything else ports **verbatim** (swap `SyncResult` → the local result type).

---

## 1. Hard-won invariants — DO NOT regress these

These are the non-obvious rules that make the code correct. They cost real
debugging time; preserve them.

1. **`for<'a> Send` on any sync reachable from an async request handler.**
   The pkcs11 app calls `uw.sync().await` *inside* axum handlers, which require
   the handler future to be `Send` **for all lifetimes**. Two things break that:
   - `futures::buffer_unordered` streams whose item futures borrow the loop item
     (`&Transaction`, `&String`). **We removed `buffer_unordered` entirely** and
     made the incremental scan sequential. **Do not reintroduce it on any
     render-path sync.** If you want concurrency, own every capture (`.cloned()`
     before `async move`) *and* verify the app (not just lib tests) compiles.
   - Holding a **borrowed** value across `.await`. Collect owned data (e.g.
     `txids: Vec<String>`) *before* the await loop; take query params by owned
     `Vec<(String,String)>`, not `&[(&str,&str)]`; own the descriptor `String`.
   > Symptom when violated: `error: implementation of Send is not general
   > enough` on every handler route — and it **only shows when the consuming app
   > is built**, not in this crate's own lib tests. Always build a consumer.

2. **`bitcoin`-free boundary.** `esplora-rs` returns `String`/int DTOs on
   purpose (no `bitcoin` dep → no version conflict). Convert to `bitcoin` types
   *only at the edge*, in the `convert` module (§3). Keep that boundary crisp.

3. **`FullScanResponse` vs `SyncResponse`.**
   - Use `FullScanResponse` (carries `last_active_indices`) whenever activity can
     appear at a **not-yet-revealed** index — i.e. the first/gap scan **and
     waterfalls** (waterfalls reports arbitrary indices). Setting
     `last_active_indices` is what makes BDK reveal up to the used index.
   - Use `SyncResponse` for the steady-state incremental path (only re-checking
     already-revealed SPKs; no new indices to reveal).

4. **`ensure_trailing_slash`.** `Url::join` silently drops the last path segment
   when the base lacks a trailing slash (`…/api` + `blocks/tip` → `…/blocks/tip`).
   Normalize the base URL once at construction (§8).

5. **Prevouts are free.** Esplora embeds each input's `prevout` (script + value),
   so BDK gets the floating `txouts` it needs for fee calc with **no extra
   requests**. Coinbase inputs carry no prevout — skip them.

6. **Refactor opportunity:** the "extend checkpoint with anchor blocks + fresh
   tip" block is duplicated 3× (rescan / incremental / waterfalls). Factor it
   into one helper here (see §9).

---

## 2. Errors — `EsploraSyncError`  (port verbatim)

```rust
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
```

> Phase-1 follow-up (tracked in esplora-rs `docs/TODO.md` E1): once esplora-rs
> exposes a structured HTTP error, thread `status`/`retry_after` through here.

---

## 3. The `convert` module — the bitcoin-free boundary  (port verbatim, incl. tests)

This is the crown jewel: every String→`bitcoin` conversion, each returning a
precise `Malformed { what, value }`. Ship it with its unit tests (they lock the
boundary and need no network).

```rust
/// `esplora-rs` String DTOs → `bitcoin` types.
mod convert {
    use bitcoin::consensus::encode::deserialize;
    use bitcoin::{Amount, BlockHash, ScriptBuf, Transaction, TxOut, Txid};
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use super::EsploraSyncError;

    pub(super) fn txid(s: &str) -> Result<Txid, EsploraSyncError> {
        s.parse().map_err(|_| EsploraSyncError::Malformed {
            what: "txid",
            value: s.to_owned(),
        })
    }

    pub(super) fn block_hash(s: &str) -> Result<BlockHash, EsploraSyncError> {
        s.parse().map_err(|_| EsploraSyncError::Malformed {
            what: "block hash",
            value: s.to_owned(),
        })
    }

    pub(super) fn tx(raw_hex: &str) -> Result<Transaction, EsploraSyncError> {
        let bytes = hex::decode(raw_hex).map_err(|_| EsploraSyncError::Malformed {
            what: "tx hex",
            value: raw_hex.chars().take(16).collect(),
        })?;
        deserialize(&bytes).map_err(|_| EsploraSyncError::Malformed {
            what: "transaction",
            value: raw_hex.chars().take(16).collect(),
        })
    }

    /// An input's previous output (script + value), from Esplora's embedded
    /// `prevout` — supplied to BDK so it can compute fees on incoming txs.
    pub(super) fn txout(prevout: &esplora_rs::models::Prevout) -> Result<TxOut, EsploraSyncError> {
        let bytes =
            hex::decode(&prevout.scriptpubkey).map_err(|_| EsploraSyncError::Malformed {
                what: "scriptpubkey hex",
                value: prevout.scriptpubkey.clone(),
            })?;
        Ok(TxOut {
            value: Amount::from_sat(prevout.value),
            script_pubkey: ScriptBuf::from_bytes(bytes),
        })
    }

    /// Build a confirmation anchor from an Esplora tx status. Returns `Ok(None)`
    /// when the tx is unconfirmed or the status is missing block fields.
    pub(super) fn anchor(
        status: &esplora_rs::TxStatus,
    ) -> Result<Option<ConfirmationBlockTime>, EsploraSyncError> {
        if !status.confirmed {
            return Ok(None);
        }
        let (Some(height), Some(hash), Some(time)) = (
            status.block_height,
            status.block_hash.as_deref(),
            status.block_time,
        ) else {
            return Ok(None);
        };
        let height = u32::try_from(height).map_err(|_| EsploraSyncError::Malformed {
            what: "block height",
            value: height.to_string(),
        })?;
        Ok(Some(ConfirmationBlockTime {
            block_id: BlockId {
                height,
                hash: block_hash(hash)?,
            },
            confirmation_time: time,
        }))
    }
}
```

Tests (port verbatim):

```rust
#[cfg(test)]
mod convert_tests {
    use super::convert;
    use bitcoin::consensus::encode::serialize_hex;
    use bitcoin::absolute::LockTime;
    use bitcoin::transaction::Version;
    use bitcoin::Transaction;

    const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn empty_tx() -> Transaction {
        Transaction { version: Version::TWO, lock_time: LockTime::ZERO, input: vec![], output: vec![] }
    }

    #[test]
    fn tx_hex_round_trips() {
        let tx = empty_tx();
        let hex = serialize_hex(&tx);
        let back = convert::tx(&hex).expect("round-trip");
        assert_eq!(back.compute_txid(), tx.compute_txid());
    }

    #[test]
    fn tx_hex_rejects_garbage() {
        assert!(convert::tx("nothex").is_err());
        assert!(convert::tx("dead").is_err());
    }

    #[test]
    fn txid_and_block_hash_parse() {
        assert!(convert::txid(ZERO_HASH).is_ok());
        assert!(convert::block_hash(ZERO_HASH).is_ok());
        assert!(convert::txid("xyz").is_err());
        assert!(convert::block_hash("xyz").is_err());
    }

    #[test]
    fn anchor_built_from_confirmed_status() {
        let status = esplora_rs::TxStatus {
            confirmed: true,
            block_height: Some(312_760),
            block_hash: Some(ZERO_HASH.to_owned()),
            block_time: Some(1_700_000_000),
        };
        let anchor = convert::anchor(&status).expect("ok").expect("some");
        assert_eq!(anchor.block_id.height, 312_760);
        assert_eq!(anchor.confirmation_time, 1_700_000_000);
    }

    #[test]
    fn txout_built_from_prevout() {
        let prevout = esplora_rs::models::Prevout {
            scriptpubkey: "00140000000000000000000000000000000000000000".to_owned(),
            scriptpubkey_asm: String::new(),
            scriptpubkey_type: "v0_p2wpkh".to_owned(),
            scriptpubkey_address: None,
            value: 50_000,
        };
        let txout = convert::txout(&prevout).expect("txout");
        assert_eq!(txout.value.to_sat(), 50_000);
        assert_eq!(txout.script_pubkey.len(), 22);
    }

    #[test]
    fn txout_rejects_bad_script_hex() {
        let prevout = esplora_rs::models::Prevout {
            scriptpubkey: "nothex".to_owned(),
            scriptpubkey_asm: String::new(),
            scriptpubkey_type: String::new(),
            scriptpubkey_address: None,
            value: 1,
        };
        assert!(convert::txout(&prevout).is_err());
    }

    #[test]
    fn anchor_none_when_unconfirmed_or_partial() {
        let unconfirmed = esplora_rs::TxStatus { confirmed: false, block_height: None, block_hash: None, block_time: None };
        assert!(convert::anchor(&unconfirmed).expect("ok").is_none());
        let partial = esplora_rs::TxStatus { confirmed: true, block_height: Some(1), block_hash: None, block_time: Some(1) };
        assert!(convert::anchor(&partial).expect("ok").is_none());
    }
}
```

---

## 4. `ingest_tx` — fold one Esplora tx into a `TxUpdate`  (port verbatim; shared by ALL scans)

The single most reusable helper: both the address scan **and** waterfalls feed
each tx through this. Fetches raw bytes once (deduped via `fetched`), attaches
prevout `txouts` for fees, and records anchor (confirmed) or `seen_at` (mempool).

```rust
/// Fold one Esplora transaction into the accumulating `TxUpdate`.
async fn ingest_tx(
    client: &esplora_rs::Client,
    tx: &esplora_rs::Transaction,
    start_time: u64,
    tx_update: &mut TxUpdate<ConfirmationBlockTime>,
    fetched: &mut BTreeSet<Txid>,
    anchor_blocks: &mut BTreeSet<BlockId>,
    new_mempool_txs: &mut u32,
) -> Result<(), EsploraSyncError> {
    let txid = convert::txid(&tx.txid)?;
    if fetched.insert(txid) {
        let raw = client.get_tx_hex(&tx.txid).await?;
        tx_update.txs.push(Arc::new(convert::tx(&raw)?));
        // Esplora embeds each input's prevout (script + value), so we can supply
        // the floating txouts BDK needs for fee calculation with no extra
        // requests. Coinbase inputs carry no prevout.
        for vin in &tx.vin {
            if let Some(prevout) = &vin.prevout {
                let outpoint = OutPoint { txid: convert::txid(&vin.txid)?, vout: vin.vout };
                tx_update.txouts.insert(outpoint, convert::txout(prevout)?);
            }
        }
    }
    if tx.status.confirmed {
        if let Some(anchor) = convert::anchor(&tx.status)? {
            anchor_blocks.insert(anchor.block_id);
            tx_update.anchors.insert((anchor, txid));
        }
    } else if tx_update.seen_ats.insert((txid, start_time)) {
        *new_mempool_txs = new_mempool_txs.saturating_add(1);
    }
    Ok(())
}
```

---

## 5. Address history helpers — raw Esplora  (port verbatim)

```rust
/// Cheap activity probe: one `address` call, no history paging.
async fn address_is_active(client: &esplora_rs::Client, address: &str) -> Result<bool, EsploraSyncError> {
    let info = client.get_address_info(address).await?;
    Ok(info.chain_stats.tx_count > 0 || info.mempool_stats.tx_count > 0)
}

/// All transactions touching `address`: confirmed history (paged) + mempool.
async fn fetch_address_txs(client: &esplora_rs::Client, address: &str) -> Result<Vec<esplora_rs::Transaction>, EsploraSyncError> {
    /// Esplora returns confirmed address history in pages of this size; a short
    /// page means the history is exhausted.
    const ESPLORA_PAGE_SIZE: usize = 25;
    let mut out = Vec::new();
    let mut last_seen: Option<String> = None;
    loop {
        let page = client.get_address_txs_chain(address, last_seen.as_deref()).await?;
        let page_len = page.len();
        if let Some(last) = page.last() {
            last_seen = Some(last.txid.clone());
        }
        out.extend(page);
        if page_len < ESPLORA_PAGE_SIZE {
            break;
        }
    }
    out.extend(client.get_address_mempool_txs(address).await?);
    Ok(out)
}
```

---

## 6. Raw-Esplora sync — `esplora_rescan` (full gap scan) + `esplora_incremental`

Dispatcher: fresh wallet (genesis checkpoint) → `rescan`; else → `incremental`.
Rescan emits a `FullScanResponse` (reveals used indices); incremental emits a
`SyncResponse`. **Both sequential** (see invariant §1.1).

```rust
pub async fn esplora_sync(wallet: &mut Wallet, backend: &EsploraBackend) -> Result<EsploraSyncResult, EsploraSyncError> {
    if wallet.latest_checkpoint().height() == 0 {
        esplora_rescan(wallet, backend).await
    } else {
        esplora_incremental(wallet, backend).await
    }
}
```

### 6a. `esplora_rescan` (port verbatim; swap `SyncResult`→`EsploraSyncResult`)

```rust
pub async fn esplora_rescan(wallet: &mut Wallet, backend: &EsploraBackend) -> Result<EsploraSyncResult, EsploraSyncError> {
    let client = backend.client();
    let start_time = now_secs();
    let base_cp = wallet.latest_checkpoint();
    let gap = backend.opts.gap_limit;

    let mut tx_update = TxUpdate::<ConfirmationBlockTime>::default();
    let mut last_active_indices = BTreeMap::<KeychainKind, u32>::new();
    let mut fetched = BTreeSet::<Txid>::new();
    let mut anchor_blocks = BTreeSet::<BlockId>::new();
    let mut new_mempool_txs = 0u32;

    for keychain in [KeychainKind::External, KeychainKind::Internal] {
        let mut index: u32 = 0;
        let mut unused_run: u32 = 0;
        loop {
            let addr_str = wallet.peek_address(keychain, index).address.to_string();
            if address_is_active(client, &addr_str).await? {
                unused_run = 0;
                last_active_indices.insert(keychain, index);
                for tx in fetch_address_txs(client, &addr_str).await? {
                    ingest_tx(client, &tx, start_time, &mut tx_update, &mut fetched, &mut anchor_blocks, &mut new_mempool_txs).await?;
                }
            } else {
                unused_run += 1;
                if unused_run >= gap { break; }
            }
            index = index.saturating_add(1);
        }
    }

    let cp = extend_checkpoint_to_tip(client, base_cp, anchor_blocks).await?; // §9
    wallet.apply_update(FullScanResponse::<KeychainKind> { tx_update, last_active_indices, chain_update: Some(cp) })?;
    Ok(finish(wallet, new_mempool_txs)) // §9
}
```

### 6b. `esplora_incremental` (port verbatim — SEQUENTIAL; this is the Send-safe rewrite)

```rust
const INCREMENTAL_LOOKAHEAD: u32 = 5;

async fn esplora_incremental(wallet: &mut Wallet, backend: &EsploraBackend) -> Result<EsploraSyncResult, EsploraSyncError> {
    let client = backend.client();
    let start_time = now_secs();
    let base_cp = wallet.latest_checkpoint();

    // Bounded target set: indices 0..=revealed(+lookahead) per keychain. Peek is
    // read-only, so collect the address strings before any &mut borrow.
    let mut targets: Vec<String> = Vec::new();
    for keychain in [KeychainKind::External, KeychainKind::Internal] {
        let last = wallet.derivation_index(keychain).unwrap_or(0);
        let hi = last.saturating_add(INCREMENTAL_LOOKAHEAD);
        for index in 0..=hi {
            targets.push(wallet.peek_address(keychain, index).address.to_string());
        }
    }

    // Sequential (staying off buffer_unordered keeps this future `for<'a> Send`).
    let mut tx_update = TxUpdate::<ConfirmationBlockTime>::default();
    let mut fetched = BTreeSet::<Txid>::new();
    let mut anchor_blocks = BTreeSet::<BlockId>::new();
    let mut new_mempool_txs = 0u32;
    for addr in &targets {
        if !address_is_active(client, addr).await? { continue; }
        for tx in fetch_address_txs(client, addr).await? {
            ingest_tx(client, &tx, start_time, &mut tx_update, &mut fetched, &mut anchor_blocks, &mut new_mempool_txs).await?;
        }
    }

    let cp = extend_checkpoint_to_tip(client, base_cp, anchor_blocks).await?; // §9
    wallet.apply_update(SyncResponse { tx_update, chain_update: Some(cp) })?;
    Ok(finish(wallet, new_mempool_txs)) // §9
}
```

> Note: the original incremental had a separate assemble loop; using `ingest_tx`
> (as above) is the cleaner unification — same behavior, one code path.

---

## 7. Waterfalls sync — `esplora_waterfalls_sync`  (port verbatim; swap result type)

One `get_waterfalls_all` per keychain returns the whole per-index history; feed
each unique txid through `ingest_tx`. Emits a `FullScanResponse`. Note the
**owned-`txids` collection before the await loop** (invariant §1.1) and that
`get_waterfalls_all` takes the descriptor **by owned `String`**.

```rust
const WATERFALLS_GAP: u32 = 20;

pub async fn esplora_waterfalls_sync(wallet: &mut Wallet, backend: &EsploraBackend) -> Result<EsploraSyncResult, EsploraSyncError> {
    let client = backend.client();
    let start_time = now_secs();
    let base_cp = wallet.latest_checkpoint();

    let mut tx_update = TxUpdate::<ConfirmationBlockTime>::default();
    let mut last_active_indices = BTreeMap::<KeychainKind, u32>::new();
    let mut fetched = BTreeSet::<Txid>::new();
    let mut anchor_blocks = BTreeSet::<BlockId>::new();
    let mut new_mempool_txs = 0u32;

    for keychain in [KeychainKind::External, KeychainKind::Internal] {
        let descriptor = wallet.public_descriptor(keychain).to_string(); // single-path e.g. wsh(...)/0/*#cks
        let to_index = wallet.derivation_index(keychain).unwrap_or(0).saturating_add(WATERFALLS_GAP);

        let resp = client.get_waterfalls_all(descriptor, to_index).await?;
        // Collect txids into an owned list BEFORE any await (no response-borrowing
        // iterator held across .await — the Send-general HRTB trap).
        let mut txids: Vec<String> = Vec::new();
        for per_index in resp.txs_seen.values() {
            for (index, sightings) in per_index.iter().enumerate() {
                if sightings.is_empty() { continue; }
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                let entry = last_active_indices.entry(keychain).or_insert(index);
                *entry = (*entry).max(index);
                txids.extend(sightings.iter().map(|s| s.txid.clone()));
            }
        }
        drop(resp);

        for txid_str in txids {
            let txid = convert::txid(&txid_str)?;
            if fetched.contains(&txid) { continue; }
            // Waterfalls gives txids only; fetch the full tx (prevouts + status),
            // then fold it in exactly like the address-scan path.
            let tx = client.get_tx(&txid_str).await?;
            ingest_tx(client, &tx, start_time, &mut tx_update, &mut fetched, &mut anchor_blocks, &mut new_mempool_txs).await?;
        }
    }

    let cp = extend_checkpoint_to_tip(client, base_cp, anchor_blocks).await?; // §9
    wallet.apply_update(FullScanResponse::<KeychainKind> { tx_update, last_active_indices, chain_update: Some(cp) })?;
    Ok(finish(wallet, new_mempool_txs)) // §9
}
```

> `get_waterfalls_all(descriptor: String, to_index: u32)` and `get_query(...,
> params: Vec<(String,String)>)` in esplora-rs are owned-param on purpose — same
> Send reason. Keep them owned.

---

## 8. Broadcast + backend/config  (port verbatim)

```rust
/// Broadcast a fully-signed transaction; returns its txid.
pub async fn esplora_broadcast(backend: &EsploraBackend, tx: &Transaction) -> Result<Txid, EsploraSyncError> {
    let hex = bitcoin::consensus::encode::serialize_hex(tx);
    let txid = backend.client().broadcast_tx(&hex).await?;
    convert::txid(&txid)
}

/// Tuning knobs. `Default` = gap limit 20, sequential.
#[derive(Debug, Clone, Copy)]
pub struct EsploraSyncOpts {
    /// Stop scanning a keychain after this many consecutive unused addresses.
    pub gap_limit: u32,
    /// Reserved for future concurrent SPK fetching. `1` = sequential (current).
    pub parallelism: usize,
}
impl Default for EsploraSyncOpts {
    fn default() -> Self { Self { gap_limit: 20, parallelism: 1 } }
}

/// A nodeless Esplora chain backend: owns an `esplora_rs::Client` + target network + opts.
#[derive(Debug, Clone)]
pub struct EsploraBackend {
    client: esplora_rs::Client,
    network: Network,
    opts: EsploraSyncOpts,
}
impl EsploraBackend {
    /// Unauthenticated public/self-hosted Esplora.
    pub fn new_public(base_url: &str, network: Network) -> Result<Self, EsploraSyncError> {
        Ok(Self { client: esplora_rs::Client::new_public(&ensure_trailing_slash(base_url))?, network, opts: EsploraSyncOpts::default() })
    }
    /// Enterprise Esplora (OAuth; reads ESPLORA_CLIENT_ID/SECRET from env — see
    /// esplora-rs E4 for the explicit-creds constructor that removes the env read).
    pub fn new_enterprise(base_url: &str, network: Network) -> Result<Self, EsploraSyncError> {
        Ok(Self { client: esplora_rs::Client::new(&ensure_trailing_slash(base_url))?, network, opts: EsploraSyncOpts::default() })
    }
    #[must_use] pub fn with_opts(mut self, opts: EsploraSyncOpts) -> Self { self.opts = opts; self }
    #[must_use] pub fn network(&self) -> Network { self.network }
    #[must_use] pub fn client(&self) -> &esplora_rs::Client { &self.client }
}

/// `Url::join` drops the last segment when the base lacks a trailing slash
/// (`…/api` + `blocks/tip` → `…/blocks/tip`). Normalize once at construction.
fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') { url.to_owned() } else { format!("{url}/") }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
```

---

## 9. Suggested new helpers (dedupe the 3× chain-update + finish blocks)

Not in the original (it was inlined 3×). Extract while porting:

```rust
/// Local result type (replaces core's `SyncResult`; core `From`-converts it).
pub struct EsploraSyncResult {
    pub changeset: bdk_wallet::ChangeSet,
    pub blocks_synced: u32,
    pub new_mempool_txs: u32,
    pub tip_height: u32,
}

/// Extend `base_cp` with every anchor block + the fresh tip.
async fn extend_checkpoint_to_tip(
    client: &esplora_rs::Client,
    base_cp: bdk_wallet::chain::local_chain::CheckPoint,
    anchor_blocks: BTreeSet<BlockId>,
) -> Result<bdk_wallet::chain::local_chain::CheckPoint, EsploraSyncError> {
    let tip_height = u32::try_from(client.get_tip_height().await?).unwrap_or(u32::MAX);
    let tip_hash = convert::block_hash(&client.get_tip_hash().await?)?;
    let mut cp = base_cp;
    for block in anchor_blocks { cp = cp.insert(block); }
    Ok(cp.insert(BlockId { height: tip_height, hash: tip_hash }))
}

/// Take the staged changeset + read the new tip height into a result.
fn finish(wallet: &mut Wallet, new_mempool_txs: u32) -> EsploraSyncResult {
    EsploraSyncResult {
        changeset: wallet.take_staged().unwrap_or_default(),
        blocks_synced: 0,
        new_mempool_txs,
        tip_height: wallet.latest_checkpoint().height(),
    }
}
```

> The original returned `changeset: wallet.take_staged()` (an `Option`) inside
> the core `SyncResult`; if the local result carries a plain `ChangeSet`, use
> `.unwrap_or_default()` as above, or keep it `Option` to match core exactly.

---

## 10. Deps this crate will need

`bdk_wallet` (3.1), `bitcoin` (0.32), `esplora-rs` (the waterfalls client),
`thiserror`, `hex`, plus `tokio`/`serde_json`/`hex` as dev-deps for the live
tests. **No `futures`** (we deliberately dropped `buffer_unordered`). Keep the
whole thing behind core's `esplora` feature.

## 11. Live tests to port (gated) — **preserved verbatim** under `reference/`

The two node-optional live tests from core are copied verbatim into
[`reference/esplora_live_signet.rs`](reference/esplora_live_signet.rs) and
[`reference/esplora_broadcast_signet.rs`](reference/esplora_broadcast_signet.rs)
so they survive the core revert. They proved the full path end-to-end (receive +
broadcast on signet). Port them as `ESPLORA_TEST_LIVE=live`-gated tests here;
the only adaptation is swapping `emvault_core::…` imports for this crate's
(and `SyncResult` → `EsploraSyncResult`). Also worth mirroring: the waterfalls
example `esplora-rs/examples/waterfalls_query.rs` (hardwired to test1's
descriptor, showed the live 100k-sat sighting).

## 12. Config / feature wiring (reference — will be *replaced*, not copied)

This is the **only genuinely non-code** change in core. Note it **changes shape**
in the new architecture: core no longer depends on `esplora-rs` directly — its
`esplora` feature now pulls in **`emvault-esplora`**, which owns the `esplora-rs`
dep. So use the block below as a *pattern*, not a paste.

**What was in `emvault-core/Cargo.toml` (old, direct):**
```toml
[features]
# Nodeless Esplora HTTP chain backend (esplora_sync + esplora_broadcast) via esplora-rs.
esplora = ["dep:esplora-rs"]

[dependencies]
esplora-rs = { version = "0.1", optional = true }   # bitcoin-agnostic String DTOs → no version conflict

[dev-dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }  # gated live tests
serde_json = "1"
hex = "0.4"

# dev-only until esplora-rs publishes the fix; remove before publishing
[patch.crates-io]
esplora-rs = { path = "../../esplora-rs" }
```

**New shape (target):**
```toml
# emvault-core/Cargo.toml
[features]
esplora = ["dep:emvault-esplora"]

[dependencies]
emvault-esplora = { path = "../emvault-esplora", optional = true }
```
```toml
# emvault-esplora/Cargo.toml — this crate owns the esplora-rs dep + dev-deps
[dependencies]
bdk_wallet = "3.1.0"
bitcoin = "0.32.10"
esplora-rs = "0.1"        # (or path/patch until published)
thiserror = "2"
hex = "0.4"
# NO futures — buffer_unordered was removed (invariant §1.1)

[dev-dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }
serde_json = "1"
hex = "0.4"
dotenvy = "0.15"          # if the live tests read a .env
```

**`emvault-core/src/lib.rs` gate (old):**
```rust
#[cfg(feature = "esplora")]
pub mod esplora_sync;
```
**New shape:** re-export from the dependency instead, e.g.
```rust
#[cfg(feature = "esplora")]
pub use emvault_esplora as esplora;   // or a thin shim mapping EsploraSyncResult → SyncResult
```

> Everything else in the core diff (`recovery.rs`, `verify.rs`, and the
> `EmVault`-backtick / import-reorder churn in `lib.rs`) is incidental
> clippy/fmt cleanup — **safe to lose**; `cargo fmt` + pedantic clippy re-create
> it. `Cargo.lock` regenerates.
