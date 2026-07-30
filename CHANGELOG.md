# Changelog

All notable changes to `emvault-esplora` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries for 0.5.0 and earlier were reconstructed from git history.
> This crate's first release was 0.3.0 (it did not exist at 0.2.0 or earlier).

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
