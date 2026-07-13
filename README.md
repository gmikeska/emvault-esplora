# emvault-esplora

Nodeless **Esplora + Waterfalls** chain backend for [EmVault](https://github.com/gmikeska/emvault-core)
(BDK) wallets — sync and broadcast a `bdk_wallet::Wallet` against a
Blockstream-style [Esplora](https://github.com/Blockstream/esplora) HTTP API, no
Bitcoin Core node required.

It is normally enabled behind `emvault-core`'s `esplora` feature (which
re-exports it as `emvault_core::esplora`), but it depends only on
`bdk_wallet` / `bitcoin` / [`esplora-rs`](https://crates.io/crates/esplora-rs),
so it can be used directly too.

```toml
[dependencies]
emvault-esplora = "0.3"
# or, via the facade:
# emvault-core = { version = "0.3", features = ["esplora"] }
```

## One backend, two strategies

A single [`EsploraBackend`] carries a [`SyncMode`]:

- **`SyncMode::Address`** — gap-limited address scan. Works on **any** Esplora
  (public or enterprise). First call on a fresh wallet full-scans; steady state
  is an incremental revealed-range scan.
- **`SyncMode::Waterfalls`** — one QuickSync / Waterfalls descriptor query per
  keychain (Blockstream **enterprise** tier). Far fewer requests, but the server
  sees the wallet **descriptor** → suitable for dev/staging, not
  privacy-sensitive production.

```rust
use emvault_esplora::{EsploraBackend, SyncMode};
use bdk_wallet::bitcoin::Network;

// inside an async fn, given a `&mut bdk_wallet::Wallet` and a signed `Transaction`:
let backend = EsploraBackend::connect("https://enterprise.blockstream.info/testnet/api", Network::Testnet)?
    .with_mode(SyncMode::Waterfalls);       // pick the strategy once

let result = backend.sync(&mut wallet).await?;   // mode-agnostic
let txid   = backend.broadcast(&signed).await?;
# Ok::<(), emvault_esplora::EsploraSyncError>(())
```

`EsploraBackend::connect` auto-selects **enterprise** (OAuth Bearer) when both
`ESPLORA_CLIENT_ID` and `ESPLORA_CLIENT_SECRET` are set (and non-empty), else
**public**. Use `new_public` / `new_enterprise` to force one explicitly.

## API surface

| Method | Purpose |
|---|---|
| `EsploraBackend::connect(url, network)` | auto public/enterprise |
| `new_public` / `new_enterprise(url, network)` | force one |
| `.with_mode(SyncMode)` | choose Address (default) or Waterfalls |
| `.with_opts(EsploraSyncOpts)` | tune the gap limit |
| `.sync(&mut wallet)` | sync using the configured mode |
| `.rescan(&mut wallet)` | force a full rescan |
| `.broadcast(&tx)` | push a signed tx (`POST /tx`) |

`sync` / `rescan` return an [`EsploraSyncResult`] (staged `ChangeSet` + counters);
`emvault-core` provides `From<EsploraSyncResult> for chain_sync::SyncResult` so
this drops into the same seam as the Bitcoin Core `emitter_sync` backend.

## Error handling

Fallible calls return `Result<_, EsploraSyncError>`. When the failure is an
Esplora HTTP error the structured detail is reachable without matching the inner
type:

```rust
use emvault_esplora::EsploraSyncError;

fn on_error(e: &EsploraSyncError) {
    if e.is_rate_limited() {
        eprintln!("rate limited; retry after {:?}s", e.retry_after());
    } else if let Some(status) = e.http_status() {
        eprintln!("esplora HTTP {status}");
    }
}
```

`EsploraSyncError` variants: `Http(esplora_rs::Error)` (transport / non-2xx /
rate-limit / decode — see [`esplora-rs`]), `Malformed { what, value }` (a value
that didn't parse into a `bitcoin` type), and `CannotConnect` (the update
couldn't connect to the wallet's local chain — usually a reorg).

## Design notes

- **`bitcoin`-free boundary.** `esplora-rs` returns `String`/int DTOs; all
  conversion to `bitcoin` types happens in one internal `convert` module, so the
  crate composes with any downstream `bitcoin`/`bdk` version.
- **`Send`-safe.** Every sync path is sequential (no `buffer_unordered`) and
  holds no borrowed value across an `.await`, so the returned futures are
  `for<'a> Send` and safe to call from inside `axum` request handlers.

## Testing

```bash
cargo test                       # unit + offline mock-server (wiremock) tests
ESPLORA_TEST_LIVE=live cargo test # + gated live signet tests (needs a node + network)
```

## License

MIT OR Apache-2.0.

[`EsploraBackend`]: https://docs.rs/emvault-esplora/latest/emvault_esplora/struct.EsploraBackend.html
[`SyncMode`]: https://docs.rs/emvault-esplora/latest/emvault_esplora/enum.SyncMode.html
[`EsploraSyncResult`]: https://docs.rs/emvault-esplora/latest/emvault_esplora/struct.EsploraSyncResult.html
[`esplora-rs`]: https://crates.io/crates/esplora-rs
