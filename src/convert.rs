//! The `bitcoin`-free boundary: every `esplora-rs` String/int DTO → `bitcoin`
//! type conversion, each returning a precise [`EsploraSyncError::Malformed`].
//!
//! Keeping all parsing here is what lets the rest of the crate (and `esplora-rs`)
//! stay free of a hard `bitcoin` version pin.

use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
use bitcoin::consensus::encode::deserialize;
use bitcoin::{Amount, BlockHash, ScriptBuf, Transaction, TxOut, Txid};

use crate::error::EsploraSyncError;

pub(crate) fn txid(s: &str) -> Result<Txid, EsploraSyncError> {
    s.parse().map_err(|_| EsploraSyncError::Malformed {
        what: "txid",
        value: s.to_owned(),
    })
}

pub(crate) fn block_hash(s: &str) -> Result<BlockHash, EsploraSyncError> {
    s.parse().map_err(|_| EsploraSyncError::Malformed {
        what: "block hash",
        value: s.to_owned(),
    })
}

pub(crate) fn tx(raw_hex: &str) -> Result<Transaction, EsploraSyncError> {
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
pub(crate) fn txout(prevout: &esplora_rs::models::Prevout) -> Result<TxOut, EsploraSyncError> {
    let bytes = hex::decode(&prevout.scriptpubkey).map_err(|_| EsploraSyncError::Malformed {
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
pub(crate) fn anchor(
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

#[cfg(test)]
mod tests {
    use super::{anchor, block_hash, tx, txid, txout};
    use bitcoin::Transaction;
    use bitcoin::absolute::LockTime;
    use bitcoin::consensus::encode::serialize_hex;
    use bitcoin::transaction::Version;

    const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn empty_tx() -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![],
        }
    }

    #[test]
    fn tx_hex_round_trips() {
        let t = empty_tx();
        let hex = serialize_hex(&t);
        let back = tx(&hex).expect("round-trip");
        assert_eq!(back.compute_txid(), t.compute_txid());
    }

    #[test]
    fn tx_hex_rejects_garbage() {
        assert!(tx("nothex").is_err());
        assert!(tx("dead").is_err());
    }

    #[test]
    fn txid_and_block_hash_parse() {
        assert!(txid(ZERO_HASH).is_ok());
        assert!(block_hash(ZERO_HASH).is_ok());
        assert!(txid("xyz").is_err());
        assert!(block_hash("xyz").is_err());
    }

    #[test]
    fn anchor_built_from_confirmed_status() {
        let status = esplora_rs::TxStatus {
            confirmed: true,
            block_height: Some(312_760),
            block_hash: Some(ZERO_HASH.to_owned()),
            block_time: Some(1_700_000_000),
        };
        let anchor = anchor(&status).expect("ok").expect("some");
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
        let txout = txout(&prevout).expect("txout");
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
        assert!(txout(&prevout).is_err());
    }

    #[test]
    fn anchor_none_when_unconfirmed_or_partial() {
        let unconfirmed = esplora_rs::TxStatus {
            confirmed: false,
            block_height: None,
            block_hash: None,
            block_time: None,
        };
        assert!(anchor(&unconfirmed).expect("ok").is_none());
        let partial = esplora_rs::TxStatus {
            confirmed: true,
            block_height: Some(1),
            block_hash: None,
            block_time: Some(1),
        };
        assert!(anchor(&partial).expect("ok").is_none());
    }
}
