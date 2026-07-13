//! Live signet proof for `EsploraBackend::broadcast` (POST `/tx`).
//!
//! Skipped unless `ESPLORA_TEST_LIVE=live`. Uses the node to build + sign a
//! fresh (un-broadcast) transaction, then pushes it through the backend —
//! proving the send direction end-to-end without a locally-confirmed BDK UTXO.
//!
//! Ported from emvault-core's `esplora_broadcast_signet.rs` (salvage §11).

use bdk_wallet::bitcoin::consensus::encode::deserialize;
use bdk_wallet::bitcoin::{Network, Transaction};
use bitcoincore_rpc::{Auth, Client as RpcClient, RpcApi};
use emvault_esplora::EsploraBackend;
use serde_json::json;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

#[tokio::test]
async fn esplora_broadcasts_a_live_signet_spend() {
    if env("ESPLORA_TEST_LIVE").as_deref() != Some("live") {
        eprintln!("skipping live signet broadcast test (set ESPLORA_TEST_LIVE=live to run)");
        return;
    }

    let esplora_url =
        env("ESPLORA_URL").unwrap_or_else(|| "https://blockstream.info/signet/api".to_owned());
    let host = env("BITCOIN_RPC_HOST").expect("BITCOIN_RPC_HOST");
    let port = env("BITCOIN_RPC_PORT").expect("BITCOIN_RPC_PORT");
    let user = env("BITCOIN_RPC_USER").expect("BITCOIN_RPC_USER");
    let pass = env("BITCOIN_RPC_PASSWORD").expect("BITCOIN_RPC_PASSWORD");
    let wallet_name = env("BITCOIN_WALLET").unwrap_or_else(|| "default".to_owned());

    // Node builds + signs a fresh tx (to its own new address) but does NOT
    // broadcast it — so the network sees it fresh when we push it.
    let rpc = RpcClient::new(
        &format!("http://{host}:{port}/wallet/{wallet_name}"),
        Auth::UserPass(user, pass),
    )
    .expect("rpc client");
    let dest: String = rpc.call("getnewaddress", &[]).expect("getnewaddress");
    let mut outputs = serde_json::Map::new();
    outputs.insert(dest, json!(0.0001));
    let raw: String = rpc
        .call("createrawtransaction", &[json!([]), json!(outputs)])
        .expect("createrawtransaction");
    let funded: serde_json::Value = rpc
        .call("fundrawtransaction", &[json!(raw)])
        .expect("fundrawtransaction");
    let funded_hex = funded["hex"].as_str().expect("funded hex").to_owned();
    let signed: serde_json::Value = rpc
        .call("signrawtransactionwithwallet", &[json!(funded_hex)])
        .expect("signrawtransactionwithwallet");
    assert_eq!(
        signed["complete"].as_bool(),
        Some(true),
        "node should fully sign"
    );
    let signed_hex = signed["hex"].as_str().expect("signed hex").to_owned();
    let tx: Transaction =
        deserialize(&hex::decode(&signed_hex).expect("hex decode")).expect("tx decode");
    let expected = tx.compute_txid();

    let backend = EsploraBackend::new_public(&esplora_url, Network::Signet).expect("backend");
    let txid = backend.broadcast(&tx).await.expect("broadcast");
    assert_eq!(txid, expected, "broadcast txid must match the signed tx");
    eprintln!("✅ broadcast accepted the live signet spend: {txid}");
}
