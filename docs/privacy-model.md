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

## Chain Data Sources

The wallet trusts its RPC providers, indexers and artifact sources for the commitment and nullifier data it syncs, and does not cross-check them against each other or against the contract. A dishonest source can hide incoming funds or show notes that don't exist, so balances can be wrong. It can't spend or redirect your private funds this way, because the Railgun contract rejects proofs built on unknown data. The wallet simulates a send before submitting it, which catches bad data unless the same provider also fakes the simulation. A self-broadcast send can then be mined and fail, and your public account still pays its gas.

## Public Broadcasters

Public broadcasters help submit private transactions without using your own public account for every transaction. A broadcaster can still observe metadata required to evaluate and relay the request it receives.

A private send or unshield that takes several transactions goes to the broadcaster as one batch. A broadcaster can submit only part of it, for example just the transaction that pays its fee, and still report the batch as submitted. Unshields that pay out the native token or add a gas top-up are bound into one execution and can't be split. The send review warns when a batch can be split, unless a remembered spend approval skips the review. Before sending again after such a send, check what arrived, since part of the batch may already have gone through.

RailOxide also monitors public broadcaster availability through Waku. In proxy mode, embedded Waku libp2p transports are disabled to prevent proxy bypass.

## Amount Privacy Hints

The Shield and Unshield forms show how many past deposits and exits used the same amount, how many deposits of the token sit in the relevant commitment tree, and a list of good and bad privacy factors.

The counts come from an amount-statistics snapshot that the indexed artifact source publishes for each chain. The wallet resolves the source's amount-statistics IPNS name and downloads the whole snapshot through the artifact gateways when a chain session starts and then once an hour. Every wallet syncing a chain from the same source downloads the same snapshot. No request depends on the selected token, an entered amount, your balances or history, or which form is open, and nothing is fetched before first-start setup finishes. The requests use the active network mode, like other artifact downloads.

The wallet checks the publisher's signature on the manifest that names the snapshot, the chain, and the snapshot's hash and size before using it, and keeps it in the public cache outside the encrypted vault. The stored snapshot holds no wallet data. A custom indexed artifact source without an amount-statistics IPNS name, or disabled indexed artifacts, means nothing is fetched. The forms then show only the factors that need no statistics: who sends the transaction, whether the recipient deposited before, how old the spent deposit is, and the network mode.

Matching against your amount, notes and history happens on your device. The hints never block an action or add a confirmation, and they never call an amount or transaction safe. They describe past public activity as the indexer reports it:

- Counts lag the chain by the indexer's confirmation depth and publish interval, plus up to an hour between wallet refreshes. A snapshot whose block is more than six hours old is shown as out of date and its counts are hidden.
- Each shield counts once. The statistics do not count distinct depositors.
- The number of deposits large enough to fund an exit is a lower bound.
- A good factor does not make a transaction unlinkable. Observers can also use timing, the gas payer, recipient history and other signals the hints do not cover.

## Browser Extension

A paired browser extension is wallet UI. It can show the selected accounts and their balances and edit chain settings, including RPC endpoints that bypass Tor. If you turn on Allow unlocking for the pairing, which is off by default, it can also unlock the wallet with the vault password and any mnemonic passphrase typed into it. Its own confirmations, such as the one for bypassing Tor, run in the browser. Settings saved from the extension stay after you remove the pairing. Pair only browsers you control, and if you remove a pairing because you stopped trusting that browser, review Settings > Chains.

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
