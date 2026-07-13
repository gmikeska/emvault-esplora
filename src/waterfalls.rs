//! Waterfalls / `QuickSync` descriptor scan: one `get_waterfalls_all` per keychain
//! returns the whole per-index history, which we fold in via the shared
//! [`ingest_tx`](crate::internal::ingest_tx). Emits a `FullScanResponse` (it can
//! report activity at not-yet-revealed indices — invariant §1.3).

use std::collections::{BTreeMap, BTreeSet};

use bdk_wallet::chain::spk_client::FullScanResponse;
use bdk_wallet::chain::{BlockId, ConfirmationBlockTime, TxUpdate};
use bdk_wallet::{KeychainKind, Wallet};
use bitcoin::Txid;

use crate::backend::EsploraBackend;
use crate::convert;
use crate::error::EsploraSyncError;
use crate::internal::{extend_checkpoint_to_tip, finish, ingest_tx, now_secs};
use crate::result::EsploraSyncResult;

/// How far past the last revealed index to ask Waterfalls to scan.
const WATERFALLS_GAP: u32 = 20;

pub(crate) async fn waterfalls_sync(
    wallet: &mut Wallet,
    backend: &EsploraBackend,
) -> Result<EsploraSyncResult, EsploraSyncError> {
    let client = backend.client();
    let start_time = now_secs();
    let base_cp = wallet.latest_checkpoint();

    let mut tx_update = TxUpdate::<ConfirmationBlockTime>::default();
    let mut last_active_indices = BTreeMap::<KeychainKind, u32>::new();
    let mut fetched = BTreeSet::<Txid>::new();
    let mut anchor_blocks = BTreeSet::<BlockId>::new();
    let mut new_mempool_txs = 0u32;

    for keychain in [KeychainKind::External, KeychainKind::Internal] {
        // Single-path descriptor, e.g. `wsh(...)/0/*#cks`.
        let descriptor = wallet.public_descriptor(keychain).to_string();
        let to_index = wallet
            .derivation_index(keychain)
            .unwrap_or(0)
            .saturating_add(WATERFALLS_GAP);

        let resp = client.get_waterfalls_all(descriptor, to_index).await?;
        // Collect txids into an owned list BEFORE any await (no response-borrowing
        // iterator held across `.await` — the Send-general HRTB trap).
        let mut txids: Vec<String> = Vec::new();
        for per_index in resp.txs_seen.values() {
            for (index, sightings) in per_index.iter().enumerate() {
                if sightings.is_empty() {
                    continue;
                }
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                let entry = last_active_indices.entry(keychain).or_insert(index);
                *entry = (*entry).max(index);
                txids.extend(sightings.iter().map(|s| s.txid.clone()));
            }
        }
        drop(resp);

        for txid_str in txids {
            let txid = convert::txid(&txid_str)?;
            if fetched.contains(&txid) {
                continue;
            }
            // Waterfalls gives txids only; fetch the full tx (prevouts + status),
            // then fold it in exactly like the address-scan path.
            let tx = client.get_tx(&txid_str).await?;
            ingest_tx(
                client,
                &tx,
                start_time,
                &mut tx_update,
                &mut fetched,
                &mut anchor_blocks,
                &mut new_mempool_txs,
            )
            .await?;
        }
    }

    let cp = extend_checkpoint_to_tip(client, base_cp, anchor_blocks).await?;
    wallet.apply_update(FullScanResponse::<KeychainKind> {
        tx_update,
        last_active_indices,
        chain_update: Some(cp),
    })?;
    Ok(finish(wallet, new_mempool_txs))
}
