//! [`EsploraSyncError`] — the crate's error type.

/// Errors raised while syncing or broadcasting through the Esplora backend.
#[derive(Debug, thiserror::Error)]
pub enum EsploraSyncError {
    /// An underlying `esplora-rs` request failed. The inner [`esplora_rs::Error`]
    /// carries the structured detail — HTTP status, rate-limit `Retry-After`, or
    /// a decode failure. See [`Self::http_status`] / [`Self::retry_after`] /
    /// [`Self::is_rate_limited`] to react without matching the inner type.
    #[error("esplora request failed: {0}")]
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

impl EsploraSyncError {
    /// The HTTP status code, when this error was a non-2xx Esplora response
    /// (`401` bad creds, `402` wrong tier, `404` not found, `5xx`, …). Rate
    /// limits (`429`) surface via [`Self::retry_after`] / [`Self::is_rate_limited`].
    #[must_use]
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Http(esplora_rs::Error::Http { status, .. }) => Some(*status),
            _ => None,
        }
    }

    /// `true` if this error was a `429 Too Many Requests` from Esplora.
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::Http(esplora_rs::Error::RateLimited { .. }))
    }

    /// The rate-limit `Retry-After` in seconds, when this was a `429` and the
    /// server supplied the header.
    #[must_use]
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::Http(esplora_rs::Error::RateLimited { retry_after, .. }) => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EsploraSyncError;

    #[test]
    fn http_status_surfaces_for_non_2xx() {
        let e = EsploraSyncError::Http(esplora_rs::Error::Http {
            status: 402,
            url: "https://enterprise/api/tx".to_string(),
            body: "payment required".to_string(),
        });
        assert_eq!(e.http_status(), Some(402));
        assert!(!e.is_rate_limited());
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn rate_limit_surfaces_retry_after() {
        let e = EsploraSyncError::Http(esplora_rs::Error::RateLimited {
            url: "https://enterprise/api".to_string(),
            retry_after: Some(9),
            body: String::new(),
        });
        assert!(e.is_rate_limited());
        assert_eq!(e.retry_after(), Some(9));
        assert_eq!(
            e.http_status(),
            None,
            "429 reports via retry_after, not http_status"
        );
    }

    #[test]
    fn non_http_errors_report_none() {
        let e = EsploraSyncError::Malformed {
            what: "txid",
            value: "xyz".to_string(),
        };
        assert_eq!(e.http_status(), None);
        assert_eq!(e.retry_after(), None);
        assert!(!e.is_rate_limited());
    }
}
