# Bitcoin Detailed

A Bitcoin mainnet solo miner with CPU and Vulkan GPU support, built in Rust and Ratatui. It receives live jobs over Stratum V1 TCP, assembles block headers locally, and submits qualifying results to the pool. The pool's node maintains the blockchain, selects transactions, and broadcasts any block it finds.

## Run

With Rust 1.89 or later, run:

```sh
cargo run --release
```

The default endpoint is `stratum.ckpool.org:3333` ([Solo CKPool](https://solo.ckpool.org/)). Paste your **public Bitcoin mainnet address** for payouts and press **Enter**. The worker name is optional. CKPool accepts `x` as the password. To use a different pool, enter a Stratum V1 TCP endpoint that supports solo mining with direct coinbase payouts.

Use **Tab** or **↓** to reach **CPU threads** on the connection screen. The default is one thread. You can enter any count up to the limit shown in the form.

Pressing **Enter** saves the pool endpoint, public address, worker, pool password, and thread count in `bitcoin-detailed.json` next to the executable. The same file holds the best hash, its discovery time in UTC, and the header that produced it. The TUI displays the discovery time in the system's local time zone. The next launch starts with the form filled in, regardless of the working directory. The file stores the pool password as plain text and is created with owner-only permissions on Unix.

When upgrading, launch once from the directory where you previously ran the miner to import its old best-hash file. The app also looks for that file next to the executable and imports settings from the previous user config location. If you run through Cargo, the executable and the JSON are in `target/debug` or `target/release`; keep a copy of the JSON before running `cargo clean`.

Mining starts paused. The **Coinbase** tab shows the output that matches your address, its value in satoshis, and the offset of its script in the transaction. Automatic mining and submissions require a positive payout to that address. You can inspect jobs and test nonces locally regardless.

| Key | Action |
| --- | --- |
| Space | Start or pause automatic mining on the selected device |
| G | Open the DEVICE tab and select a device. Enter a listed GPU number or `0` for CPU |
| I | Change GPU intensity from 1% to 100%. Press Enter to apply and save it |
| C | Change the CPU thread count without pausing or reconnecting. Press Enter to apply and save it |
| N | Test a nonce on the displayed job. Enter decimal or hexadecimal, such as `0x1234`. The attempt pauses automatic mining |
| 1–7, Tab | Switch between Mine, Header, Coinbase, Merkle, Transactions, Best, and DEVICE |
| B | Toggle hexadecimal and binary. In binary, 32-byte hashes and targets use four rows of eight bytes when the terminal is wide enough |
| T | Request the job's TXIDs from the pool |
| ↑ / ↓, PgUp / PgDn | Navigate the TXID list or scroll the tab |
| Enter / Esc in Transactions | Look up the selected transaction in Esplora or return to the list |
| Q, Ctrl+C | Quit |

The miner sends every qualifying attempt, manual or automatic, via `mining.submit`. The share target measures work for the pool. The network target, decoded from `nBits`, sets the proof-of-work threshold for a block candidate. Even after the pool accepts a candidate, the network must accept the block for a payout. A CPU or GPU has an extremely small chance of hitting that target.

The **Mine** tab shows the network difficulty derived from the job's `nBits`, alongside the network target and the current hash. On wide terminals the two values appear side by side, with the first differing byte highlighted in each. The **Header** tab also shows the difficulty next to the compact target.

## GPU mining

On Linux and Windows, the **DEVICE** tab lists hardware adapters with a Vulkan compute driver. AMD, NVIDIA, and Intel use the same bundled compute shader. Install the Vulkan-capable driver for your card. The TUI handles mining and the pool connection directly. CPU mining remains available when the app finds no compatible GPU.

Each session starts with CPU selected. Use **G** to select one GPU. Switching devices keeps the pool connection, current job, and nonce progress. Automatic mining uses one device at a time, and manual nonce tests run on the CPU. The **Mine**, **Header**, and **Best** tabs work with results from either device.

Press **I** to set GPU intensity while mining. At **50%**, the miner waits for about as long as each burst took before submitting more work. Lower values leave more idle time between bursts. The percentage controls the miner's duty cycle. Actual draw, temperature, and hashrate depend on the GPU and its driver. The default is **100%**, and the app remembers your selection in `bitcoin-detailed.json`.

The Vulkan shader reuses the SHA-256 state through the first three rounds of the second 64-byte block. The nonce first enters in round four. Two buffer sets allow the GPU to hash one batch while the CPU reads the previous batch. Each batch covers up to 1,048,576 nonces. The miner uses smaller batches to establish the initial record or handle unusually easy share targets. The shader returns the last hash and a bitmap of nonces eligible for submission or for a new record. The CPU recomputes those hashes and checks the exact targets before updating the display or submitting a share.

To check the shader on your Vulkan hardware, run:

```sh
cargo test --release vulkan_matches_cpu -- --ignored --nocapture
```

To compare Vulkan batch and workgroup sizes on your hardware, run:

```sh
cargo test --release vulkan_benchmark_tuning -- --ignored --nocapture
```

## What goes into the hash

```text
coinb1 || extranonce1 || extranonce2 || coinb2
  → coinbase transaction, with your payout output
  → coinbase TXID combined with the Merkle branch
  → Merkle root
  → header: version || prevhash || merkle root || nTime || nBits || nonce
  → SHA-256(SHA-256(80-byte header))
  → is the hash, read as a 256-bit integer, at or below the target?
```

The **Header** tab shows each field's bytes and purpose, both SHA-256 digests, and the byte order used to display and compare the hash. The miner caches the SHA-256 state after the first 64 bytes of the header and lets each thread take its next range of nonces independently. The **Merkle** tab shows the intermediate hashes. When the 32-bit nonce runs out, the miner increments extranonce2 in the coinbase, calculates a new Merkle root, and rebuilds the cached state.

## What the pool provides

The `mining.notify` message supplies the coinbase in parts and a Merkle branch. That is enough to mine. Viewing individual transactions requires more data. If the pool implements [`mining.get_txnhashes`](https://github.com/ckolivas/ckpool/blob/master/src/stratifier.c), press **T** to request that job's TXIDs. The app checks the list and its ordering against the Merkle branch and root. Availability depends on the pool's configuration, and mining can continue without the list.

To inspect a transaction, select its TXID and press **Enter**. The TUI requests the raw bytes from the [public Esplora API](https://github.com/Blockstream/esplora/blob/master/API.md), verifies the TXID, and shows the decoded fields and serialized bytes. Esplora receives the requested TXID and may not have an unconfirmed transaction. The TXID authenticates the bytes excluding witness data. The app displays Esplora's witness data without checking it against the block's SegWit commitment.

A node or indexer is needed to browse the full transaction history or a node's mempool. Each node has its own mempool. A node's [`getblocktemplate`](https://github.com/bitcoin/bips/blob/master/bip-0022.mediawiki) returns the raw transactions in **that node's template**, which can differ from the Stratum pool's selection. The TUI displays the data it can obtain and verify for the current job.
