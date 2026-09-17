# RailOxide Privacy Model

RailOxide is privacy-oriented, but metadata privacy still depends on the network mode, POI source, and infrastructure choices you make.

The recommended default posture is built-in Tor with indexed POI artifacts.

## Recommended Defaults

By default, RailOxide uses:

- Built-in Tor for wallet HTTP/RPC traffic.
- Indexed POI artifacts for normal POI reads.
- Official POI artifact publisher and gateway settings.

This default avoids sending wallet blinded commitments to the POI proxy for normal POI status and proof reads. The wallet downloads signed POI artifact snapshots, keeps a local POI cache, and uses that local cache when checking received UTXOs or preparing spends.

## Network Modes

RailOxide has three network modes.

| Mode | Privacy posture | Notes |
| --- | --- | --- |
| Built-in Tor | Recommended default | Routes wallet HTTP/RPC traffic through the bundled Tor client. A system Tor daemon is not required. |
| Proxy | User-controlled | Routes wallet HTTP/RPC traffic through the configured proxy. Embedded Waku libp2p transports are disabled in proxy mode to avoid proxy bypass. |
| Direct | Privacy-degraded | Sends outbound requests over the normal network. Use only when you intentionally accept that remote services can see your network address. |

Direct mode is explicit because it exposes more metadata to services the wallet contacts.

## POI Sources

RailOxide supports two POI read sources.

| POI source | Recommended | What it exposes |
| --- | --- | --- |
| Indexed artifacts | Yes | Downloads signed public POI snapshots and uses local cache reads for normal wallet POI checks. The POI RPC URL is still used to live-tail recent public POI events. |
| POI proxy | No, unless you trust the operator | Sends blinded commitment hashes associated with UTXOs you are receiving or preparing to spend to the POI RPC operator. |

Use indexed artifacts unless you have a specific reason to trust and use a POI proxy directly.

## What Remote Services Can Observe

RailOxide reduces unnecessary leaks, but remote services can still observe metadata for the requests they receive.

Services that may observe request metadata include:

- RPC providers, including providers queried for public balances, gas quotes, public-account actions, and on-chain token price-oracle reads.
- POI services.
- Artifact gateways.
- Public broadcasters.
- Waku peers.

Token metadata is built in or user-configured. Price anchors for evaluating transaction fees are read from on-chain oracles through configured RPC providers, not through a separate token telemetry service.

Self-broadcast and public-account actions may preflight or submit against multiple configured RPC providers for reliability. Each selected provider can observe the public transaction metadata it receives.

Artifact gateways can observe artifact downloads, including timing and requested artifact paths. With the recommended Tor or proxy modes, those requests are routed through the selected network path. In direct mode, gateways can also observe your network address.

## Public Broadcasters

Public broadcasters help submit private transactions without using your own public account for every transaction. A broadcaster can still observe metadata required to evaluate and relay the request it receives.

RailOxide also monitors public broadcaster availability through Waku. In proxy mode, embedded Waku libp2p transports are disabled to prevent proxy bypass.

## Sender PPOI Submissions

Prepared sender PPOI submissions belong to the chain sync service. The wallet hands off proofs after saving its encrypted recovery records, so submissions can finish after switching wallets, including switches before confirmation. Output positions come from the existing public chain log stream; this workflow does not poll transaction receipts. Active-wallet submissions share the same transport and deduplication state.

The chain retains prepared proofs in memory for up to 30 minutes and retries submission failures for up to 30 minutes. Vault lock, shutdown, and public-cache reset cancel this work. Encrypted recovery records remain authoritative for recovery when the sending wallet is reopened; submission alone does not mark an output spendable.

## Wallet Storage

The encrypted wallet vault protects wallet secrets and decrypted-wallet cache material. Wallet UTXO payloads, pending-output POI contexts, and output POI recovery records are encrypted with wallet view capability before they enter the local database. Their workflow-row keys are opaque, domain-separated HMAC identifiers rather than transaction hashes, commitments, NPKs, or output roles. Authentication binds each encrypted payload to its record kind, wallet-chain namespace, and opaque row ID.

Lower-sensitivity coordination metadata remains plaintext by product decision. This includes the wallet-chain ownership index, `wallet_meta` checkpoint and safe-head fields, and wallet sync actor/reset state. These records can reveal local wallet organization and synchronization progress, but not the encrypted transaction-correlatable POI workflow payloads.

Released plaintext pending-output and recovery rows migrate after the matching view capability is unlocked and before the wallet actor starts. Hardware-derived wallets therefore require the matching hardware view capability. Migration is retryable and fail-closed: runtime reads do not mix old plaintext rows with encrypted rows, and malformed or conflicting rows stop startup for that wallet-chain namespace.

Logical migration removes plaintext rows from current database tables and atomically schedules database compaction. RailOxide does not swap or compact a live shared database handle; compaction runs before handles are exposed on the next cold database open. Until that restart completes, freed pages and pre-migration backups may still contain old plaintext bytes.

App settings are stored outside the encrypted vault and may include:

- Proxy URLs.
- RPC endpoints.
- POI RPC URLs.
- Waku endpoints.
- Custom infrastructure settings.

Avoid putting credentials or API tokens in URLs when possible.

## Logs

RailOxide UI logs are intended for non-sensitive diagnostics. Logs redact URL credentials, paths, query strings, and fragments where possible, but users should still avoid entering sensitive values in URLs or settings fields.

Never share logs publicly without reviewing them first.
