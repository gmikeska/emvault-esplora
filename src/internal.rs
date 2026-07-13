//! Scan-shared helpers used by both the address and waterfalls paths:
//! folding a tx into a [`TxUpdate`], extending the checkpoint to the tip, and
//! finishing into an [`EsploraSyncResult`].

use std::collections::BTreeSet;
use std::sync::Arc;

use bdk_wallet::Wallet;
use bdk_wallet::chain::{BlockId, CheckPoint, ConfirmationBlockTime, TxUpdate};
use bitcoin::{OutPoint, Txid};

use crate::convert;
use crate::error::EsploraSyncError;
use crate::result::EsploraSyncResult;

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Fold one Esplora transaction into the accumulating [`TxUpdate`].
///
/// Fetches the raw bytes once (deduped via `fetched`), attaches each input's
/// prevout `txout` for fee calculation, and records either a confirmation
/// anchor or a mempool `seen_at`.
pub(crate) async fn ingest_tx(
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
                let outpoint = OutPoint {
                    txid: convert::txid(&vin.txid)?,
                    vout: vin.vout,
                };
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

/// Extend `base_cp` with every anchor block plus the fresh chain tip.
pub(crate) async fn extend_checkpoint_to_tip(
    client: &esplora_rs::Client,
    base_cp: CheckPoint,
    anchor_blocks: BTreeSet<BlockId>,
) -> Result<CheckPoint, EsploraSyncError> {
    let tip_height = u32::try_from(client.get_tip_height().await?).unwrap_or(u32::MAX);
    let tip_hash = convert::block_hash(&client.get_tip_hash().await?)?;
    let mut cp = base_cp;
    for block in anchor_blocks {
        cp = cp.insert(block);
    }
    Ok(cp.insert(BlockId {
        height: tip_height,
        hash: tip_hash,
    }))
}

/// Take the staged changeset and read the new tip height into a result.
pub(crate) fn finish(wallet: &mut Wallet, new_mempool_txs: u32) -> EsploraSyncResult {
    EsploraSyncResult {
        changeset: wallet.take_staged(),
        blocks_synced: 0,
        new_mempool_txs,
        tip_height: wallet.latest_checkpoint().height(),
    }
}
