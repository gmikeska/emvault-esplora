//! Offline control-flow tests for both sync strategies, driven by a `wiremock`
//! mock server — no network, no node. These exercise the full
//! `EsploraBackend::sync` path (address gap scan / waterfalls descriptor scan),
//! the checkpoint-to-tip extension, and result shaping against canned Esplora
//! responses.

use bdk_wallet::Wallet;
use bdk_wallet::bitcoin::hashes::Hash;
use bdk_wallet::bitcoin::{
    Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
    absolute::LockTime, consensus, transaction::Version,
};
use emvault_esplora::{EsploraBackend, EsploraSyncError, SyncMode};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A valid 64-hex block hash for the mocked chain tip.
const TIP_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const TIP_HEIGHT: &str = "100";

/// bdk's published test wpkh descriptors (testnet tprv) — deterministic and
/// self-contained, no signing needed for a receive-side scan.
fn test_wallet() -> Wallet {
    let external = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/0/*)";
    let internal = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/1/*)";
    Wallet::create(external.to_string(), internal.to_string())
        .network(Network::Testnet)
        .create_wallet_no_persist()
        .expect("create test wallet")
}

/// Esplora `AddressInfo` JSON for an address with `n` confirmed transactions.
fn address_info(n: u64) -> serde_json::Value {
    let stats = serde_json::json!({
        "tx_count": n,
        "funded_txo_count": 0,
        "funded_txo_sum": 0,
        "spent_txo_count": 0,
        "spent_txo_sum": 0,
    });
    serde_json::json!({
        "address": "tb1qtest",
        "chain_stats": stats,
        "mempool_stats": {
            "tx_count": 0,
            "funded_txo_count": 0,
            "funded_txo_sum": 0,
            "spent_txo_count": 0,
            "spent_txo_sum": 0,
        },
    })
}

/// Mount the chain-tip endpoints every sync calls at the end, plus the
/// `/block-height/<h>` endpoint the steady-state reorg reconciliation queries
/// (`EsploraBackend::server_reorg_below_tip`). Returning `TIP_HASH` for every
/// height means the wallet's stored checkpoint (also `TIP_HASH`) reconciles as a
/// match → no reorg → no spurious rebuild on an incremental sync.
async fn mount_tip(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/blocks/tip/height"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TIP_HEIGHT))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/blocks/tip/hash"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TIP_HASH))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/block-height/\d+$"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TIP_HASH))
        .mount(server)
        .await;
}

#[tokio::test]
async fn address_scan_empty_wallet_advances_tip() {
    let server = MockServer::start().await;
    mount_tip(&server).await;
    // Every probed address is unused → the gap scan terminates cleanly.
    Mock::given(method("GET"))
        .and(path_regex(r"^/address/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(address_info(0)))
        .mount(&server)
        .await;

    let mut wallet = test_wallet();
    let backend = EsploraBackend::new_public(&server.uri(), Network::Testnet).expect("backend");
    let result = backend.sync(&mut wallet).await.expect("sync");

    assert_eq!(
        result.tip_height, 100,
        "tip should advance to the mocked height"
    );
    assert_eq!(result.new_mempool_txs, 0);
    assert_eq!(
        wallet.balance().total().to_sat(),
        0,
        "empty wallet, no funds"
    );
    // Second (incremental) pass on a non-genesis wallet must stay consistent.
    // (The `/block-height` mock returns the matching hash → no spurious reorg.)
    let again = backend.sync(&mut wallet).await.expect("incremental sync");
    assert_eq!(again.tip_height, 100);
    assert!(
        !again.reorg_rebuilt,
        "a matching checkpoint hash must NOT trigger a rebuild"
    );
}

/// The other half of the reconciliation truth table: when the server reports a
/// **different** block hash at a stored checkpoint height than the wallet holds,
/// `server_reorg_below_tip` must detect the reorg-below-tip and rebuild. Proves the
/// detector *fires* (offline complement to the live 2×2), not just that it stays
/// quiet on a match.
#[tokio::test]
async fn address_incremental_detects_reorg_and_rebuilds() {
    // A different hash at the stored checkpoint height than the wallet holds.
    const REORG_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000002";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/blocks/tip/height"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TIP_HEIGHT))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/blocks/tip/hash"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TIP_HASH))
        .mount(&server)
        .await;
    // All addresses unused → both the initial gap scan and the post-detection
    // rebuild's full scan terminate cleanly.
    Mock::given(method("GET"))
        .and(path_regex(r"^/address/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(address_info(0)))
        .mount(&server)
        .await;

    let mut wallet = test_wallet();
    let backend = EsploraBackend::new_public(&server.uri(), Network::Testnet).expect("backend");
    // First sync stores a checkpoint at height 100 = TIP_HASH.
    backend.sync(&mut wallet).await.expect("initial sync");

    // The server now reports a DIFFERENT hash at that height → reorg-below-tip.
    Mock::given(method("GET"))
        .and(path_regex(r"^/block-height/\d+$"))
        .respond_with(ResponseTemplate::new(200).set_body_string(REORG_HASH))
        .mount(&server)
        .await;

    let again = backend.sync(&mut wallet).await.expect("incremental sync");
    assert!(
        again.reorg_rebuilt,
        "a checkpoint-hash mismatch at height <= tip must trigger a rebuild"
    );
    assert_eq!(
        again.tip_height, 100,
        "wallet re-followed the tip after rebuild"
    );
}

#[tokio::test]
async fn waterfalls_scan_empty_wallet_advances_tip() {
    let server = MockServer::start().await;
    mount_tip(&server).await;
    // Waterfalls: one descriptor query per keychain, reporting no activity.
    let empty_waterfalls = serde_json::json!({
        "txs_seen": { "descriptor": [[]] },
        "page": 0,
        "tip": TIP_HASH,
    });
    Mock::given(method("GET"))
        .and(path("/waterfalls/v2/waterfalls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_waterfalls))
        .mount(&server)
        .await;

    let mut wallet = test_wallet();
    let backend = EsploraBackend::new_public(&server.uri(), Network::Testnet)
        .expect("backend")
        .with_mode(SyncMode::Waterfalls);
    let result = backend.sync(&mut wallet).await.expect("waterfalls sync");

    assert_eq!(result.tip_height, 100);
    assert_eq!(result.new_mempool_txs, 0);
    assert_eq!(wallet.balance().total().to_sat(), 0);
}

#[tokio::test]
async fn address_scan_surfaces_http_errors() {
    // A 500 from the tip endpoint must surface as an error, not a silent 0-tip.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/address/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(address_info(0)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/blocks/tip/height"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let mut wallet = test_wallet();
    let backend = EsploraBackend::new_public(&server.uri(), Network::Testnet).expect("backend");
    assert!(
        backend.sync(&mut wallet).await.is_err(),
        "HTTP 500 must propagate"
    );
}

/// A deterministic, unsigned transaction — self-contained and reproducible so
/// its `compute_txid()` is stable across runs. No signing is needed: the offline
/// tests only exercise fetch + consensus decode, not validation.
fn sample_tx() -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::all_zeros(), 0),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::new(),
        }],
    }
}

#[tokio::test]
async fn tip_height_reads_chain_tip() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/blocks/tip/height"))
        .respond_with(ResponseTemplate::new(200).set_body_string("100"))
        .mount(&server)
        .await;

    let backend = EsploraBackend::new_public(&server.uri(), Network::Testnet).expect("backend");
    assert_eq!(
        backend.tip_height().await.expect("tip height"),
        100,
        "tip_height must read the plaintext chain tip"
    );
}

#[tokio::test]
async fn get_tx_fetches_and_decodes() {
    // esplora-rs `get_raw_tx` hits `/tx/{txid}/raw` and returns RAW CONSENSUS
    // BYTES — so the mock body is the serialized tx, not hex.
    let tx = sample_tx();
    let txid = tx.compute_txid();
    let raw = consensus::serialize(&tx);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tx/{txid}/raw")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(raw))
        .mount(&server)
        .await;

    let backend = EsploraBackend::new_public(&server.uri(), Network::Testnet).expect("backend");
    let fetched = backend.get_tx(txid).await.expect("get_tx");
    assert_eq!(
        fetched.compute_txid(),
        txid,
        "decoded transaction must round-trip to the same txid"
    );
}

#[tokio::test]
async fn get_tx_missing_returns_error() {
    let txid = sample_tx().compute_txid();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tx/{txid}/raw")))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let backend = EsploraBackend::new_public(&server.uri(), Network::Testnet).expect("backend");
    assert!(
        backend.get_tx(txid).await.is_err(),
        "a 404 on /tx/{{txid}}/raw must surface as an error"
    );
}

#[tokio::test]
async fn get_tx_malformed_bytes_error() {
    let txid = sample_tx().compute_txid();
    let server = MockServer::start().await;
    // A 200 whose body is not valid consensus-encoded bytes → decode failure.
    Mock::given(method("GET"))
        .and(path(format!("/tx/{txid}/raw")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xde, 0xad, 0xbe, 0xef]))
        .mount(&server)
        .await;

    let backend = EsploraBackend::new_public(&server.uri(), Network::Testnet).expect("backend");
    let err = backend
        .get_tx(txid)
        .await
        .expect_err("malformed bytes must error");
    assert!(
        matches!(err, EsploraSyncError::Malformed { .. }),
        "non-decodable bytes must map to EsploraSyncError::Malformed, got: {err:?}"
    );
}
