//! Address-based Esplora scan: full gap scan on a fresh wallet, incremental
//! (bounded revealed-range) scan thereafter. Both are **sequential** to keep the
//! returned future `for<'a> Send` (invariant §1.1).

use std::collections::{BTreeMap, BTreeSet};

use bdk_wallet::chain::spk_client::{FullScanResponse, SyncResponse};
use bdk_wallet::chain::{BlockId, ConfirmationBlockTime, TxUpdate};
use bdk_wallet::{KeychainKind, Wallet};
use bitcoin::Txid;

use crate::backend::EsploraBackend;
use crate::error::EsploraSyncError;
use crate::internal::{extend_checkpoint_to_tip, finish, ingest_tx, now_secs};
use crate::result::EsploraSyncResult;

/// How far past the last revealed index the incremental scan probes, to catch
/// activity on addresses BDK revealed but hasn't yet marked used.
const INCREMENTAL_LOOKAHEAD: u32 = 5;

/// Dispatch: fresh wallet (genesis checkpoint) → full rescan; else incremental.
pub(crate) async fn esplora_sync(
    wallet: &mut Wallet,
    backend: &EsploraBackend,
) -> Result<EsploraSyncResult, EsploraSyncError> {
    if wallet.latest_checkpoint().height() == 0 {
        esplora_rescan(wallet, backend).await
    } else {
        esplora_incremental(wallet, backend).await
    }
}

/// Full gap scan of both keychains → `FullScanResponse` (reveals used indices).
pub(crate) async fn esplora_rescan(
    wallet: &mut Wallet,
    backend: &EsploraBackend,
) -> Result<EsploraSyncResult, EsploraSyncError> {
    let client = backend.client();
    let start_time = now_secs();
    let base_cp = wallet.latest_checkpoint();
    let gap = backend.gap_limit();

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
            } else {
                unused_run += 1;
                if unused_run >= gap {
                    break;
                }
            }
            index = index.saturating_add(1);
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

/// Incremental scan of the bounded revealed range → `SyncResponse`. Sequential
/// (staying off `buffer_unordered` keeps this future `for<'a> Send`).
async fn esplora_incremental(
    wallet: &mut Wallet,
    backend: &EsploraBackend,
) -> Result<EsploraSyncResult, EsploraSyncError> {
    let client = backend.client();
    let start_time = now_secs();
    let base_cp = wallet.latest_checkpoint();

    // Bounded target set: indices 0..=revealed(+lookahead) per keychain. Peek is
    // read-only, so collect the owned address strings before any &mut borrow.
    let mut targets: Vec<String> = Vec::new();
    for keychain in [KeychainKind::External, KeychainKind::Internal] {
        let last = wallet.derivation_index(keychain).unwrap_or(0);
        let hi = last.saturating_add(INCREMENTAL_LOOKAHEAD);
        for index in 0..=hi {
            targets.push(wallet.peek_address(keychain, index).address.to_string());
        }
    }

    let mut tx_update = TxUpdate::<ConfirmationBlockTime>::default();
    let mut fetched = BTreeSet::<Txid>::new();
    let mut anchor_blocks = BTreeSet::<BlockId>::new();
    let mut new_mempool_txs = 0u32;
    for addr in &targets {
        if !address_is_active(client, addr).await? {
            continue;
        }
        for tx in fetch_address_txs(client, addr).await? {
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
    wallet.apply_update(SyncResponse {
        tx_update,
        chain_update: Some(cp),
    })?;
    Ok(finish(wallet, new_mempool_txs))
}

/// Cheap activity probe: one `address` call, no history paging.
async fn address_is_active(
    client: &esplora_rs::Client,
    address: &str,
) -> Result<bool, EsploraSyncError> {
    let info = client.get_address_info(address).await?;
    Ok(info.chain_stats.tx_count > 0 || info.mempool_stats.tx_count > 0)
}

/// All transactions touching `address`: confirmed history (paged) + mempool.
async fn fetch_address_txs(
    client: &esplora_rs::Client,
    address: &str,
) -> Result<Vec<esplora_rs::Transaction>, EsploraSyncError> {
    /// Esplora returns confirmed address history in pages of this size; a short
    /// page means the history is exhausted.
    const ESPLORA_PAGE_SIZE: usize = 25;
    let mut out = Vec::new();
    let mut last_seen: Option<String> = None;
    loop {
        let page = client
            .get_address_txs_chain(address, last_seen.as_deref())
            .await?;
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
