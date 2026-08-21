# Changelog

All notable changes to `emvault-esplora` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries for 0.5.0 and earlier were reconstructed from git history.
> This crate's first release was 0.3.0 (it did not exist at 0.2.0 or earlier).

## [0.9.0] - 2026-08-21

### Changed
- Released in lockstep with the suite-wide v0.9.0 (driven by `emvault-elements`'
  asset-aware federation migration). No functional changes to `emvault-esplora`
  this round; adds GitHub CI workflows and switches inter-crate dependencies to
  version-only requirements so isolated CI resolves against crates.io.

## [0.8.0] - 2026-08-16

### Changed
- Lockstep version bump to stay in sync with the emvault suite. No functional
  changes since 0.7.0.

## [0.7.0] - 2026-08-03

### Added
- `EsploraBackend::tip_height()` — read the current chain tip from the Esplora /
  Waterfalls REST backend without a bitcoind RPC node.
- `EsploraBackend::get_tx(txid)` — fetch a full transaction via `/tx/{txid}`
  (raw bytes, decoded to `bitcoin::Transaction`). Enables nodeless
  previous-transaction lookups (e.g. Trezor sign-data prev-tx fetch), so no
  consumer code path needs RPC when running in esplora/waterfalls mode.

### Changed
- Released in lockstep with the suite-wide v0.7.0 update.

## [0.6.0] - 2026-07-29

### Changed
- Repin to `esplora-rs` 0.3, which widens `TxSeen.v` to `i64` to accept the
  waterfalls spend-side `v: -1` sentinel (previously 500'd the `/v2` body).

### Added
- Reorg-reconciliation support in the Esplora / Waterfalls sync path.

## [0.5.0] - 2026-07-27

### Changed
- Dependency and lockfile refresh; version realigned across the emvault suite.

## [0.4.0] - 2026-07-22

### Added
- Crate `README.md`.

### Changed
- Release-metadata bump.

## [0.3.0] - 2026-07-13

Initial release.

### Added
- Nodeless Esplora + Waterfalls BDK chain backend (`EsploraBackend`, `SyncMode`).
- Surfaced `http_status` / `retry_after` on request errors.
- Offline mock-server sync tests, port-gated live signet tests, and backend unit tests.

### Changed
- Targets `esplora-rs` 0.2. **BREAKING** relative to pre-release internals
  (error surface reshaped).
