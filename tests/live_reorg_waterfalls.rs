//! Live regtest reorg matrix for `emvault-esplora` — **both** `SyncMode`s ×
//! **both** reorg states, against **real** indexers (no facade, no reshaping).
//!
//! ## Topology (production nginx + esplora + waterfalls, all real)
//! - regtest bitcoind — JSON-RPC + REST `host.docker.internal:18543`
//!   (`regtest`/`regtest`, `-rest`, `-txindex=1`), wallet `miner`.
//! - **blockstream electrs** (esplora HTTP API) indexing that node — serves
//!   `/address`, `/tx`, `/blocks/tip`, mempool, … (`ELECTRS_URL`, default `:3000`).
//! - **RCasatta waterfalls** server (node mode) indexing the SAME node — serves
//!   `/v2/waterfalls` (`:3105`).
//! - a **pure** reverse proxy (nginx role: routes `/waterfalls/*` → waterfalls,
//!   everything else → electrs) so Waterfalls mode sees both under one base URL
//!   (`WATERFALLS_URL`, default `:3106`). It reshapes nothing.
//!
//! ## The 2×2 matrix
//! `SyncMode::{Address, Waterfalls}` × state `{ (a) permanent eviction, (b) mempool
//! re-queue }`. Each asserts the CORRECT (electrum/RPC-convergent) behaviour, keyed
//! on the specific funding UTXO `D` + its `chain_position` (same discipline as the
//! electrum/RPC tests).
//!
//! ## FINDING (2026-07-29) — bug diagnosed here, then FIXED; all four now GREEN
//! Standing up a REAL esplora indexer (blockstream electrs) first confirmed the
//! earlier facade result was **not a facade artifact**: `emvault-esplora` did **not**
//! detect a reorg-below-tip, uniformly across **both** modes and **both** states
//! (Address ≡ Waterfalls — no mode divergence). `sync()` returned
//! `reorg_rebuilt=false` and never reached post-reorg ground truth. Root cause,
//! shared by `sync.rs` and `waterfalls.rs`: [`extend_checkpoint_to_tip`] only
//! *extends* the wallet's own checkpoint chain (base_cp + new anchors + new tip) and
//! never re-fetches the hash at existing checkpoint heights, so a strictly-longer
//! reorg never flipped a checkpoint hash for the old wallet-only pre/post compare to
//! catch (the wallet kept the ORPHANED hashes — the in-test DIAG shows this).
//! electrum escapes only because `bdk_electrum` re-derives checkpoints from the
//! server.
//!
//! **Fix (landed):** `EsploraBackend::server_reorg_below_tip` now reconciles the
//! wallet's stored checkpoint hashes against the server's actual block hash at each
//! height (all stored checkpoints `<= h_pre`, no depth ceiling) — the same
//! ground-truth guarantee `bdk_electrum` gets natively. A silent reorg is now caught
//! and the D2/D3/D5 rebuild fires on both modes and both states.
//!
//! Observed matrix AFTER the fix (real electrs + waterfalls, fresh compile) — GREEN:
//! - **state (a)** (both modes): `reorg_rebuilt=true evicted=[D] balance=0` — the
//!   reorged-out D is evicted entirely; the phantom UTXO is cleared. Converges with
//!   electrum/RPC.
//! - **state (b)** (both modes): `reorg_rebuilt=true evicted=[] pending=500000000`,
//!   D present as `Unconfirmed` — the rebuild re-scans and re-surfaces the re-queued
//!   mempool tx, which is (correctly, D5) NOT evicted. Converges with electrum/RPC.
//!
//! All four assert the correct convergent behaviour and are now the passing
//! regression guard for the fix. They are `WATERFALLS_LIVE`-gated, so the offline
//! suite and lints are unaffected.
//!
//! ## Run
//! ```text
//! WATERFALLS_LIVE=1 cargo test --test live_reorg_waterfalls -- --nocapture --test-threads=1
//! ```
//! Overrides: `WATERFALLS_URL` (router), `ELECTRS_URL`, `WATERFALLS_DIRECT`,
//! `BITCOIND_RPC`, `BITCOIND_RPC_AUTH`, `MINER_WALLET`.

// Live integration harness: the same pedantic-lint noise as the electrum sibling.
#![allow(
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::collapsible_if,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::process::Command;
use std::time::{Duration, Instant};

use bdk_wallet::bitcoin::bip32::Xpriv;
use bdk_wallet::bitcoin::{Network, Txid};
use bdk_wallet::chain::ChainPosition;
use bdk_wallet::{KeychainKind, Wallet};
use emvault_esplora::{EsploraBackend, SyncMode};
use serde_json::{Value, json};

/// A **fresh** wpkh descriptor pair per test run (unique regtest master key), so
/// re-runs never collide on the shared chain — each run funds virgin scripts. Seed is
/// time+pid derived (test isolation only, not cryptographic); neutral, no personal
/// data. The wallet holds the private descriptor; the scan uses `public_descriptor`.
fn fresh_descriptors() -> (String, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mix = nanos ^ (u128::from(std::process::id()) << 96);
    let mut seed = [0u8; 32];
    for (i, b) in seed.iter_mut().enumerate() {
        *b = ((mix >> ((i % 16) * 8)) as u8) ^ (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    let xprv = Xpriv::new_master(Network::Regtest, &seed).expect("master xprv");
    (
        format!("wpkh({xprv}/84h/1h/0h/0/*)"),
        format!("wpkh({xprv}/84h/1h/0h/1/*)"),
    )
}

const FUND_SATS: u64 = 500_000_000; // 5 BTC
const EVICT_FEE_SATS: u64 = 1_000_000; // 0.01 BTC — D' dwarfs D's fee, RBF-replacing it

fn skip() -> bool {
    if std::env::var("WATERFALLS_LIVE").is_err() {
        eprintln!("SKIP: set WATERFALLS_LIVE=1 to run the live regtest reorg matrix");
        return true;
    }
    false
}

/// Base URL emvault points at for a given mode: Waterfalls → the unifying router
/// (esplora + `/waterfalls`); Address → electrs directly.
fn backend_url(mode: SyncMode) -> String {
    match mode {
        SyncMode::Waterfalls => {
            std::env::var("WATERFALLS_URL").unwrap_or_else(|_| "http://127.0.0.1:3106".to_string())
        }
        SyncMode::Address => electrs_url(),
    }
}
fn electrs_url() -> String {
    std::env::var("ELECTRS_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_string())
}
fn waterfalls_direct() -> String {
    std::env::var("WATERFALLS_DIRECT").unwrap_or_else(|_| "http://127.0.0.1:3105".to_string())
}
fn rpc_base() -> String {
    std::env::var("BITCOIND_RPC")
        .unwrap_or_else(|_| "http://host.docker.internal:18543".to_string())
}
fn rpc_auth() -> String {
    std::env::var("BITCOIND_RPC_AUTH").unwrap_or_else(|_| "regtest:regtest".to_string())
}
fn miner_path() -> String {
    format!(
        "/wallet/{}",
        std::env::var("MINER_WALLET").unwrap_or_else(|_| "miner".to_string())
    )
}

fn mode_name(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::Address => "Address",
        SyncMode::Waterfalls => "Waterfalls",
    }
}

/// Minimal bitcoind JSON-RPC over `curl` (mirrors `drive.sh`). Panics on error.
fn rpc(method: &str, params: Value, wallet: Option<&str>) -> Value {
    let url = format!("{}{}", rpc_base(), wallet.unwrap_or(""));
    let body = json!({"jsonrpc": "1.0", "id": "wf-reorg", "method": method, "params": params});
    let out = Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "60",
            "--user",
            &rpc_auth(),
            "--data-binary",
            &body.to_string(),
            "-H",
            "content-type: text/plain;",
            &url,
        ])
        .output()
        .expect("spawn curl");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "rpc {method}: bad JSON: {e}: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert!(
        v.get("error").is_none_or(Value::is_null),
        "rpc {method} error: {}",
        v["error"]
    );
    v["result"].clone()
}

fn block_count() -> u64 {
    rpc("getblockcount", json!([]), None).as_u64().unwrap()
}
fn block_hash(height: u64) -> String {
    rpc("getblockhash", json!([height]), None)
        .as_str()
        .unwrap()
        .to_string()
}
fn block_height_of(hash: &str) -> u64 {
    rpc("getblockheader", json!([hash, true]), None)["height"]
        .as_u64()
        .unwrap()
}
fn new_miner_addr() -> String {
    rpc("getnewaddress", json!([]), Some(&miner_path()))
        .as_str()
        .expect("getnewaddress")
        .to_string()
}
/// Mine `n` blocks that *include mempool txs* (normal `generatetoaddress`).
fn mine(n: u32) -> u64 {
    let addr = new_miner_addr();
    rpc("generatetoaddress", json!([n, addr]), None);
    block_count()
}
/// Mine one **coinbase-only** block (no mempool txs) via `generateblock` — used to
/// build the state-(b) competing branch so the re-queued funding tx is never mined.
fn mine_empty_block() {
    let addr = new_miner_addr();
    rpc("generateblock", json!([addr, []]), None);
}

/// GET a URL via `curl`, returning the body. `--globoff` because descriptors carry
/// `[origin/path]` brackets that curl would otherwise treat as glob patterns.
fn http_get(url: &str) -> String {
    let out = Command::new("curl")
        .args(["-sg", "--max-time", "30", url])
        .output()
        .expect("spawn curl");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Wait until electrs's indexed tip reaches `target`.
fn wait_electrs_tip(target: u64) {
    let url = format!("{}/blocks/tip/height", electrs_url());
    let deadline = Instant::now() + Duration::from_mins(3);
    loop {
        if let Ok(h) = http_get(&url).trim().parse::<u64>() {
            if h >= target {
                eprintln!("electrs indexed tip caught up: {h} (>= {target})");
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "electrs indexer did not reach height {target} within 180s"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Wait until the waterfalls server's indexed tip reaches `target` (its
/// `/blocks/tip/hash` mapped to a height via bitcoind).
fn wait_waterfalls_tip(target: u64) {
    let url = format!("{}/blocks/tip/hash", waterfalls_direct());
    let deadline = Instant::now() + Duration::from_mins(3);
    loop {
        let hash = http_get(&url);
        let hash = hash.trim();
        if hash.len() == 64 && block_height_of(hash) >= target {
            eprintln!("waterfalls indexed tip caught up: {target}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "waterfalls indexer did not reach height {target} within 180s"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Wait until whichever indexer the mode reads has caught up to `target`.
fn wait_indexer(mode: SyncMode, target: u64) {
    wait_electrs_tip(target); // both modes read the tip from electrs (router → electrs)
    if mode == SyncMode::Waterfalls {
        wait_waterfalls_tip(target); // Waterfalls mode also reads sightings from waterfalls
    }
}

/// Does the Waterfalls descriptor scan for `desc` currently list `txid`?
fn wf_scan_has_txid(desc: &str, txid: &str) -> bool {
    let url = format!(
        "{}/v2/waterfalls?descriptor={desc}&to_index=30",
        waterfalls_direct()
    );
    let Ok(v) = serde_json::from_str::<Value>(&http_get(&url)) else {
        return false;
    };
    v["txs_seen"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .flat_map(|per_index| per_index.as_array().into_iter().flatten())
        .flat_map(|sightings| sightings.as_array().into_iter().flatten())
        .any(|s| s["txid"].as_str() == Some(txid))
}

/// Does electrs's `/address/<addr>/txs` (confirmed + mempool) currently list `txid`?
fn electrs_addr_has_txid(addr: &str, txid: &str) -> bool {
    let url = format!("{}/address/{addr}/txs", electrs_url());
    let Ok(v) = serde_json::from_str::<Value>(&http_get(&url)) else {
        return false;
    };
    v.as_array()
        .into_iter()
        .flatten()
        .any(|t| t["txid"].as_str() == Some(txid))
}

/// Wait until the mode's indexer reflects the expected presence of `txid` for the
/// funding script (`addr` for Address, `desc` for Waterfalls).
fn wait_scan(mode: SyncMode, addr: &str, desc: &str, txid: &str, want: bool) {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let seen = match mode {
            SyncMode::Address => electrs_addr_has_txid(addr, txid),
            SyncMode::Waterfalls => wf_scan_has_txid(desc, txid),
        };
        if seen == want {
            eprintln!("[{}] scan settled: {txid} present={want}", mode_name(mode));
            return;
        }
        assert!(
            Instant::now() < deadline,
            "[{}] scan did not reach present={want} for {txid} within 45s",
            mode_name(mode)
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn btc(sats: u64) -> f64 {
    sats as f64 / 100_000_000.0
}

/// State captured by [`fund_and_bury`] and handed to each state's reorg body.
struct Funded {
    wallet: Wallet,
    backend: EsploraBackend,
    addr: String,
    ext_query: String,
    d_txid: String,
    u_txid: String,
    u_vout: u64,
    u_value_btc: f64,
    h0: u64,
    b0: String,
    h_pre: u64,
}

/// Fund a fresh wallet address with 5 BTC, confirm + bury it, sync so the backend
/// persists a checkpoint above the funding block.
async fn fund_and_bury(mode: SyncMode) -> Funded {
    let backend = EsploraBackend::new_public(&backend_url(mode), Network::Regtest)
        .expect("connect backend")
        .with_mode(mode);
    let (ext, int) = fresh_descriptors();
    let mut wallet = Wallet::create(ext, int)
        .network(Network::Regtest)
        .create_wallet_no_persist()
        .expect("fresh regtest wallet");
    let ext_query = wallet.public_descriptor(KeychainKind::External).to_string();

    let addr = wallet.reveal_next_address(KeychainKind::External).address;
    let addr = addr.to_string();
    eprintln!("[{}] funding address: {addr}", mode_name(mode));
    let d_txid = rpc(
        "sendtoaddress",
        json!([addr, btc(FUND_SATS)]),
        Some(&miner_path()),
    );
    let d_txid = d_txid.as_str().expect("sendtoaddress txid").to_string();
    let d = rpc("getrawtransaction", json!([d_txid, true]), None);
    let u_txid = d["vin"][0]["txid"].as_str().unwrap().to_string();
    let u_vout = d["vin"][0]["vout"].as_u64().unwrap();
    let u = rpc("getrawtransaction", json!([u_txid, true]), None);
    let u_value_btc = u["vout"][u_vout as usize]["value"].as_f64().unwrap();
    eprintln!("[{}] D={d_txid} U={u_txid}:{u_vout}", mode_name(mode));

    let h0 = mine(1);
    let b0 = block_hash(h0);
    let h_pre = mine(2);
    eprintln!("[{}] D in B0@{h0}; pre-reorg tip={h_pre}", mode_name(mode));

    wait_indexer(mode, h_pre);
    wait_scan(mode, &addr, &ext_query, &d_txid, true);
    let mut wallet_ref = wallet;
    let r1 = backend.sync(&mut wallet_ref).await.expect("sync #1");
    let bal1 = wallet_ref.balance().total().to_sat();
    eprintln!(
        "[{}] sync #1: tip={} balance={bal1}",
        mode_name(mode),
        r1.tip_height
    );
    assert_eq!(bal1, FUND_SATS, "wallet should see the funding UTXO");
    assert_eq!(
        u64::from(r1.tip_height),
        h_pre,
        "persisted tip == pre-reorg tip"
    );

    Funded {
        wallet: wallet_ref,
        backend,
        addr,
        ext_query,
        d_txid,
        u_txid,
        u_vout,
        u_value_btc,
        h0,
        b0,
        h_pre,
    }
}

/// state (a) — permanent eviction: D double-spent, absent entirely, evicted.
async fn run_state_a(mode: SyncMode) {
    let Funded {
        mut wallet,
        backend,
        addr,
        ext_query,
        d_txid,
        u_txid,
        u_vout,
        u_value_btc,
        h0,
        b0,
        h_pre,
        ..
    } = fund_and_bury(mode).await;
    let d: Txid = d_txid.parse().expect("D txid");

    rpc("invalidateblock", json!([b0]), None); // D -> mempool
    let dest = new_miner_addr();
    let out_sats = (u_value_btc * 100_000_000.0).round() as u64 - EVICT_FEE_SATS;
    let mut outputs = serde_json::Map::new();
    outputs.insert(dest, json!(btc(out_sats)));
    let raw = rpc(
        "createrawtransaction",
        json!([[{"txid": u_txid, "vout": u_vout}], Value::Object(outputs)]),
        None,
    );
    let signed = rpc(
        "signrawtransactionwithwallet",
        json!([raw.as_str().unwrap()]),
        Some(&miner_path()),
    );
    assert!(
        signed["complete"].as_bool().unwrap_or(false),
        "sign D': {signed}"
    );
    let d_prime = rpc(
        "sendrawtransaction",
        json!([signed["hex"].as_str().unwrap()]),
        None,
    );
    eprintln!(
        "[{}] broadcast D' (evicts D): {}",
        mode_name(mode),
        d_prime.as_str().unwrap()
    );
    let h_post = mine((h_pre - block_count()) as u32 + 3);
    assert!(h_post > h_pre, "reorg branch must be strictly longer");

    wait_indexer(mode, h_post);
    wait_scan(mode, &addr, &ext_query, &d_txid, false); // D gone (D' replaced it)

    let r2 = backend.sync(&mut wallet).await.expect("sync #2 (reorg)");
    let bal = wallet.balance().total().to_sat();
    eprintln!(
        "[{}] state (a) post-reorg sync: reorg_rebuilt={} evicted={:?} tip={} balance={bal}",
        mode_name(mode),
        r2.reorg_rebuilt,
        r2.evicted_txids,
        r2.tip_height
    );
    for h in [h0, h_pre] {
        let node = block_hash(h);
        let w = wallet
            .local_chain()
            .get(u32::try_from(h).unwrap())
            .map(|cp| cp.hash().to_string());
        eprintln!(
            "[{}] DIAG chain@{h}: node={node} wallet={w:?} {}",
            mode_name(mode),
            if w.as_deref() == Some(node.as_str()) {
                "MATCH"
            } else {
                "STALE(orphaned)"
            }
        );
    }

    assert!(
        r2.reorg_rebuilt,
        "[{}] sync() must flag a reorg-below-tip rebuild",
        mode_name(mode)
    );
    assert_eq!(
        bal,
        0,
        "[{}] rebuild must clear the phantom UTXO",
        mode_name(mode)
    );
    assert_eq!(
        r2.evicted_txids,
        vec![d],
        "[{}] D must be reported evicted",
        mode_name(mode)
    );
    assert_eq!(
        u64::from(r2.tip_height),
        h_post,
        "wallet followed reorg tip"
    );
    assert!(
        wallet.get_tx(d).is_none(),
        "[{}] D absent from rebuilt graph (D5)",
        mode_name(mode)
    );
    assert!(
        wallet.list_unspent().next().is_none(),
        "no spendable outputs"
    );
    assert_ne!(block_hash(h0), b0, "funding block genuinely replaced");

    let r3 = backend.sync(&mut wallet).await.expect("post-rebuild sync");
    assert!(
        !r3.reorg_rebuilt,
        "follow-up sync must not re-trigger a rebuild"
    );
    assert!(
        r3.evicted_txids.is_empty(),
        "follow-up sync reports no evictions"
    );
    assert_eq!(wallet.balance().total().to_sat(), 0, "balance stays 0");
    eprintln!(
        "[{}] state (a) CONCLUSION: converges with electrum/RPC.",
        mode_name(mode)
    );
}

/// state (b) — mempool re-queue: D returned to mempool, present as Unconfirmed,
/// NOT evicted (does the indexer surface the mempool tx?).
async fn run_state_b(mode: SyncMode) {
    let Funded {
        mut wallet,
        backend,
        addr,
        ext_query,
        d_txid,
        h0,
        b0,
        h_pre,
        ..
    } = fund_and_bury(mode).await;
    let d: Txid = d_txid.parse().expect("D txid");

    rpc("invalidateblock", json!([b0]), None);
    let mempool = rpc("getrawmempool", json!([]), None);
    assert!(
        mempool
            .as_array()
            .is_some_and(|a| a.iter().any(|t| t.as_str() == Some(&d_txid))),
        "[{}] state (b): D must be back in the mempool: {mempool}",
        mode_name(mode)
    );
    let need = (h_pre - block_count()) + 3;
    for _ in 0..need {
        mine_empty_block();
    }
    let h_post = block_count();
    assert!(h_post > h_pre, "reorg branch must be strictly longer");
    let mempool = rpc("getrawmempool", json!([]), None);
    assert!(
        mempool
            .as_array()
            .is_some_and(|a| a.iter().any(|t| t.as_str() == Some(&d_txid))),
        "[{}] state (b): D must remain unconfirmed in the mempool: {mempool}",
        mode_name(mode)
    );

    wait_indexer(mode, h_post);
    wait_scan(mode, &addr, &ext_query, &d_txid, true); // indexer must still surface D (mempool)

    let r2 = backend.sync(&mut wallet).await.expect("sync #2 (reorg)");
    let total = wallet.balance().total().to_sat();
    let pending = wallet.balance().untrusted_pending.to_sat();
    eprintln!(
        "[{}] state (b) post-reorg sync: reorg_rebuilt={} evicted={:?} tip={} total={total} pending={pending}",
        mode_name(mode),
        r2.reorg_rebuilt,
        r2.evicted_txids,
        r2.tip_height
    );

    assert!(
        r2.reorg_rebuilt,
        "[{}] state (b): reorg-below-tip still triggers a rebuild",
        mode_name(mode)
    );
    assert_eq!(
        u64::from(r2.tip_height),
        h_post,
        "wallet followed reorg tip"
    );
    assert!(
        r2.evicted_txids.is_empty(),
        "[{}] state (b): a re-queued mempool tx must NOT be evicted, got {:?}",
        mode_name(mode),
        r2.evicted_txids
    );
    let wtx = wallet
        .transactions()
        .find(|w| w.tx_node.txid == d)
        .expect("state (b): D must be present in the rebuilt graph");
    assert!(
        matches!(wtx.chain_position, ChainPosition::Unconfirmed { .. }),
        "[{}] state (b): D must be Unconfirmed, got {:?}",
        mode_name(mode),
        wtx.chain_position
    );
    assert_eq!(
        pending,
        FUND_SATS,
        "[{}] re-queued funding shows as untrusted-pending",
        mode_name(mode)
    );
    assert_ne!(block_hash(h0), b0, "funding block genuinely replaced");
    eprintln!(
        "[{}] state (b) CONCLUSION: mempool tx surfaced, NOT reverted — converges.",
        mode_name(mode)
    );

    let _ = mine(1); // drain the deliberate mempool leftover
}

// ---------------------------------------------------------------------------
// The 2×2 matrix. Serialize with `--test-threads=1` (shared chain).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn address_reorg_state_a_permanent_eviction() {
    if skip() {
        return;
    }
    run_state_a(SyncMode::Address).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn address_reorg_state_b_mempool_requeue() {
    if skip() {
        return;
    }
    run_state_b(SyncMode::Address).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waterfalls_reorg_state_a_permanent_eviction() {
    if skip() {
        return;
    }
    run_state_a(SyncMode::Waterfalls).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waterfalls_reorg_state_b_mempool_requeue() {
    if skip() {
        return;
    }
    run_state_b(SyncMode::Waterfalls).await;
}
