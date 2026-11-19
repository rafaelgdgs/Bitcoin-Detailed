# Bitcoin Detailed

A Bitcoin mainnet solo CPU miner with a terminal interface built in Rust and Ratatui. It receives live jobs over Stratum V1 TCP, assembles block headers locally, and submits qualifying results to the pool. The pool's node maintains the blockchain, selects transactions, and broadcasts any block it finds. To run the miner, all you need is the TUI and an internet connection.

## Run

With Rust 1.89 or later, run:

```sh
cargo run --release
```

The default endpoint is `stratum.ckpool.org:3333` ([Solo CKPool](https://solo.ckpool.org/)). Paste your **public Bitcoin mainnet address** for payouts and press **Enter**. The worker name is optional. CKPool accepts `x` as the password. To use a different pool, enter a Stratum V1 TCP endpoint that supports solo mining with direct coinbase payouts.

Use **Tab** or **↓** to reach **CPU threads** on the connection screen. The default is one thread. You can enter any count up to the limit shown in the form.

Pressing **Enter** saves the pool endpoint, public address, worker, pool password, and thread count in `bitcoin-detailed.json` next to the executable. The same file holds the best hash, its discovery time in UTC, and the header that produced it. The next launch starts with the form filled in, regardless of the working directory. The file stores the pool password as plain text and is created with owner-only permissions on Unix.

When upgrading, launch once from the directory where you previously ran the miner to import its old best-hash file. The app also looks for that file next to the executable and imports settings from the previous user config location. If you run through Cargo, the executable and the JSON are in `target/debug` or `target/release`; keep a copy of the JSON before running `cargo clean`.

Mining starts paused. The **Coinbase** tab shows the output that matches your address, its value in satoshis, and the offset of its script in the transaction. Automatic mining and submissions require a positive payout to that address. You can inspect jobs and test nonces locally regardless.

| Key | Action |
| --- | --- |
| Space | Start or pause automatic mining using the configured CPU threads |
| C | Change the CPU thread count without pausing or reconnecting. Press Enter to apply and save it |
| N | Test a nonce on the displayed job. Enter decimal or hexadecimal, such as `0x1234`. The attempt pauses automatic mining |
| 1–6, Tab | Switch between Mine, Header, Coinbase, Merkle, Transactions, and Best |
| B | Toggle hexadecimal and binary. In binary, 32-byte hashes and targets use four rows of eight bytes when the terminal is wide enough |
| T | Request the job's TXIDs from the pool |
| ↑ / ↓, PgUp / PgDn | Navigate the TXID list or scroll the tab |
| Enter / Esc in Transactions | Look up the selected transaction in Esplora or return to the list |
| Q, Ctrl+C | Quit |

The miner sends every qualifying attempt, manual or automatic, via `mining.submit`. The share target measures work for the pool. The network target, decoded from `nBits`, sets the proof-of-work threshold for a block candidate. Even after the pool accepts a candidate, the network must accept the block for a payout. A CPU has an extremely small chance of hitting that target.

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

The **Header** tab shows each field's bytes and purpose, both SHA-256 digests, and the byte order used to display and compare the hash. The miner caches the SHA-256 state after the first 64 bytes of the header and gives each thread its own range of nonces. The **Merkle** tab shows the intermediate hashes. When the 32-bit nonce runs out, the miner increments extranonce2 in the coinbase, calculates a new Merkle root, and rebuilds the cached state.

## What the pool provides

The `mining.notify` message supplies the coinbase in parts and a Merkle branch. That is enough to mine. Viewing individual transactions requires more data. If the pool implements [`mining.get_txnhashes`](https://github.com/ckolivas/ckpool/blob/master/src/stratifier.c), press **T** to request that job's TXIDs. The app checks the list and its ordering against the Merkle branch and root. Availability depends on the pool's configuration, and mining can continue without the list.

To inspect a transaction, select its TXID and press **Enter**. The TUI requests the raw bytes from the [public Esplora API](https://github.com/Blockstream/esplora/blob/master/API.md), verifies the TXID, and shows the decoded fields and serialized bytes. Esplora receives the requested TXID and may not have an unconfirmed transaction. The TXID authenticates the bytes excluding witness data. The app displays Esplora's witness data without checking it against the block's SegWit commitment.

A node or indexer is needed to browse the full transaction history or a node's mempool. Each node has its own mempool. A node's [`getblocktemplate`](https://github.com/bitcoin/bips/blob/master/bip-0022.mediawiki) returns the raw transactions in **that node's template**, which can differ from the Stratum pool's selection. The TUI displays the data it can obtain and verify for the current job.
