# Basic Rollup

A single-app SOL-transfer rollup demo. One Rust server contains the HTTP API,
sequencer, Agave SVM execution, account state and transaction receipts. A separate
client signs and submits transfers. No other service is required.

The application is native SOL transfers: it accepts legacy, v0 and v1 transactions
containing only System `Transfer` instructions. It is not a general
program-execution platform. Settlement on a Solana cluster is optional and described
in [Settlement](#settlement): without it the rollup runs exactly as before.

## Requirements

- Rust 1.98.1 or newer. Nothing else: no Solana CLI and no validator.

## Run

Run every command from the project folder:

```sh
cd Basic_Rollup
```

Create the demo keys and a genesis file that funds the payer. They are saved in
`demo/`, which git ignores, and are reused by every later run and by the test tasks:

```sh
cargo xtask demo
```

It prints the commands below with the recipient's address filled in. Start the
server. It keeps running in this terminal:

```sh
ROLLUP_GENESIS=demo/genesis.json cargo run -p rollup_core --locked
```

In a second terminal, from the same folder, send 100000000 lamports (0.1 SOL) to the
address that `cargo xtask demo` printed:

```sh
cargo run -p rollup_client --locked -- demo/payer.json RECIPIENT_ADDRESS 100000000
```

The client prints the receipt. Read the recipient's balance, which is now
100000000 lamports:

```sh
curl http://127.0.0.1:8080/get_account/RECIPIENT_ADDRESS
```

- Amounts are integer lamports.
- The client's optional last argument is the rollup URL. It defaults to
  `http://127.0.0.1:8080`. `ROLLUP_BIND` changes the address the server listens on.
- The client signs a v1 transaction whose message config states the compute units
  and loaded account data a transfer needs.
- Accounts and receipts are in memory. Restarting the server resets them.

### The genesis file

Genesis is JSON containing `accounts`, each with a unique `address` and a `u64`
`lamports` balance. It creates System-owned accounts and cannot replace the System
program account. Its balances are synthetic lamports, not deposits. The SVM enforces
Mainnet's rent rules, so a newly funded recipient must receive at least 650240
lamports, the rent-exempt minimum of an empty account.

A Solana cluster starts the same way: its genesis config (`genesis.bin`, produced by
`solana-genesis`) holds the first funded accounts, the builtin programs and the
cluster parameters, and a validator builds its first bank from it.

### Pulling accounts from a cluster

Set `ROLLUP_CLUSTER_URL` to a Solana RPC URL (devnet, Mainnet or a local validator)
and the rollup copies an account from that cluster the first time a transaction or
a balance query touches it:

```sh
ROLLUP_CLUSTER_URL=https://api.devnet.solana.com cargo run -p rollup_core --locked
```

An account is pulled once. From then on the rollup reads and changes only its own
copy, so later changes on the cluster are not seen. With a cluster configured,
`ROLLUP_GENESIS` is optional, and a genesis account takes precedence over the
cluster's copy. Nothing is written back to the cluster and nothing locks the
account there: without [settlement](#settlement), the same lamports can still be spent
on the cluster.

## Run the tests

From the project folder, one command formats the code, runs Clippy with warnings
denied, runs every test and then the smoke test:

```sh
cd Basic_Rollup
```

```sh
cargo xtask verify
```

It needs no Solana CLI and no network. It ends with `Smoke test passed`. The smoke
test takes its keys from `demo/` and creates the ones that are missing.

The parts can be run on their own:

| Command | What it does | Needs |
| --- | --- | --- |
| `cargo test --workspace --locked` | The tests alone | Nothing else |
| `cargo xtask smoke` | Runs the real server and client: two identical transfers, exact balances, clean shutdown. Report in `target/verification/cli/` | Nothing else |
| `cargo xtask mainnet` | Compares the rent rate and the inactive features in `sequencer.rs` with Mainnet | Network |
| `cargo xtask live` | Starts the rollup on a real cluster and checks that a pulled account matches it. `ROLLUP_TEST_CLUSTER_URL` selects the cluster, default devnet | Network |

Run one test by its name:

```sh
cargo test -p rollup_core --locked failed_transactions_pay_the_fee_and_change_nothing_else
```

The tests in `rollup_core/tests/http_rollup.rs` start the real server on loopback
and execute signed transfers in the SVM. Each prints the path of its genesis and
server log.

| Test | What it checks |
| --- | --- |
| `legacy_transfers_commit_state_and_reject_replays` | Balances, fees, new recipients, repeated payments with fresh blockhashes, replays |
| `v1_transactions_carry_their_budget_and_reach_4096_bytes` | v1 budgets and priority fees, a 60-recipient payout, the size limit |
| `failed_transactions_pay_the_fee_and_change_nothing_else` | A failed transaction keeps only its fee payment |
| `invalid_submissions_are_rejected_without_a_fee` | Bad signatures, messages, blockhashes and non-transfer instructions |
| `accounts_are_pulled_from_the_cluster_once` | Pulling from a stand-in cluster |

## How it works

The rollup executes with **Agave SVM 4.4.1**, the validator release Solana Mainnet
runs under Solana 1.0. Sanitization, signature verification, budget and fee
derivation, account loading and execution run through the same crates a validator
uses (`solana-runtime-transaction`, `solana-fee`, `solana-svm`). The Agave crates
are pinned to one version in the workspace `Cargo.toml` and need the
`agave-unstable-api` feature.

Comments marked `Agave:` in `rollup_core/src` link each phase of the sequencer to
the code that does the same job in the validator, at tag v4.4.1: the banking-stage
scheduler, account locking, preparing the SVM, executing a transaction in it and
committing the result.

The diagrams are in [docs/architecture](docs/architecture/README.md).

### Mainnet parameters

The rollup executes one transaction at a time on a single thread, but every limit
it enforces is the Mainnet value, taken from the official crates wherever they
export it. The values live at the top of `rollup_core/src/sequencer.rs`.

| Parameter | Value | Source |
| --- | --- | --- |
| Transaction size, v1 | 4096 bytes | `solana_message::v1::MAX_TRANSACTION_SIZE` |
| Transaction size, legacy and v0 | 1232 bytes | `solana_packet::PACKET_DATA_SIZE` |
| v1 accounts, instructions, signatures | 64, 64, 12 | `solana_message::v1` sanitization |
| Account locks per transaction | 64 | Mainnet has not activated the raise to 128 |
| Fee per signature | 5000 lamports | `FeeStructure::default()`, via `solana_fee` |
| Priority fee | v1: total lamports in the message config | `solana_runtime_transaction` |
| Compute budget | v1: from the message config, unset means 0 | `solana_runtime_transaction` |
| Compute budget, legacy and v0 | 3000 units per builtin instruction | `solana_compute_budget_instruction` |
| Rent | 5080 lamports per byte, 650240 for an empty account | `set_lamports_per_byte_to_5080` |
| Blockhash validity | the 150 most recent hashes | `solana_clock::MAX_PROCESSING_AGE` |
| Feature set | every Agave 4.4.1 feature except 37 not yet active | snapshot of 2026-11-16 |

Two values are a snapshot of the live cluster rather than a crate constant: the
rent rate and the list of inactive features. `cargo xtask mainnet` compares both
with Mainnet and fails when either has changed.

### Differences from a validator

- No blocks or slots. A fresh blockhash is issued per request.
- No address lookup tables.
- No ComputeBudget program. Legacy and v0 transactions run with the default limits;
  v1 states its own.
- Fees are removed from the paying account without being burned or paid to a leader.
- Settlement is optional. Without it receipts report `settlement: "not_configured"`; with it they report `"pending"`.

## Settlement

Settlement connects the rollup to a Solana cluster: lamports used in the rollup are
committed on the cluster, and a wallet leaves with the balance it ended up with. The
task is in [docs/settlement-challenge.md](docs/settlement-challenge.md); the answers
to its nine decisions, the comparison with MagicBlock's delegation program and the
full list of limits are in [docs/settlement-design.md](docs/settlement-design.md).

How it works, in short:

- `programs/settlement/` is a small on-chain program (own workspace, built with
  `cargo build-sbf`). A wallet **deposits** lamports into a vault account owned by the
  program; a per-wallet ledger account records the deposit. The wallet's own account is
  never assigned, so it keeps paying its fees.
- With `ROLLUP_SETTLEMENT_PROGRAM` (program id) and `ROLLUP_SEQUENCER_KEYPAIR` (path to
  the sequencer's keypair file) set, the rollup starts in settlement mode. Both are
  needed, `ROLLUP_CLUSTER_URL` is required and `ROLLUP_GENESIS` must not be set. The
  rollup starts a wallet with its deposit and refuses, without a fee, any
  transaction that touches an address that has not deposited.
- A wallet **leaves** with `POST /leave`. The sequencer sends one cluster transaction in
  which the program pays out the wallet's final rollup balance, and only after the
  cluster confirms does the rollup forget the wallet.

```sh
# program id and sequencer: use your own keys; the cluster here is a local validator
cargo build-sbf --manifest-path programs/settlement/Cargo.toml
cargo run -q -p rollup_client -- initialize sequencer.json <PROGRAM_ID>
cargo run -q -p rollup_client -- deposit wallet.json <PROGRAM_ID> 1000000000
ROLLUP_CLUSTER_URL=http://127.0.0.1:8899 ROLLUP_SETTLEMENT_PROGRAM=<PROGRAM_ID> \
  ROLLUP_SEQUENCER_KEYPAIR=sequencer.json cargo run -p rollup_core --locked
cargo run -q -p rollup_client -- wallet.json <RECIPIENT> 500000000   # inside the rollup
cargo run -q -p rollup_client -- leave wallet.json                   # back to the cluster
```

`cargo xtask settle` runs the whole flow on a local validator (it needs
`solana-test-validator` and `cargo-build-sbf`, so `cargo xtask verify` does not run it):
two wallets deposit, one pays the other, the receiver leaves first, the payer later,
with owners and exact lamports checked after every step, and with the refusals the
design relies on.

Limits, briefly (the full list is in [docs/settlement-design.md](docs/settlement-design.md)):
the sequencer is trusted and there are no proofs; the first `initialize` decides who
the sequencer is; rollup fees stay in the vault and cannot be claimed; rollup state is in
memory, so after a restart balances are rebuilt from the deposits and transfers made
inside the rollup are lost; one deposit per wallet until it leaves; the rollup waits
while a `leave` is confirmed on the cluster.

## HTTP API

| Method and path | Input | Successful output |
| --- | --- | --- |
| `GET /` | None | Health, application, settlement status |
| `GET /get_latest_blockhash` | None | A fresh rollup `blockhash` |
| `GET /get_account/{address}` | Base58 public key | `address`, `lamports` (pulled from the cluster if unseen) |
| `POST /submit_transaction` | `{"sol_transaction": "<base64 serialized transaction>"}` | Execution receipt |
| `POST /get_transaction` | `{"get_tx": "<transaction signature>"}` | Stored execution receipt |
| `POST /leave` | `{"wallet": "<address>", "blockhash": "<rollup blockhash>", "signature": "<base58>"}` | `wallet`, `lamports` paid, `cluster_signature` (settlement mode only) |

`sol_transaction` is the base64 of the serialized transaction, the same bytes
Solana's `sendTransaction` takes. Receipts are looked up by the first transaction
signature. Submission waits for execution and state commit.

A receipt contains `signature`, `version` (`"legacy"`, `0` or `1`), `status`
(`"executed"` or `"failed"`), `error` when it failed, `fee`,
`compute_units_consumed`, `settlement` and `sol_transaction`.

| HTTP status | Meaning |
| --- | --- |
| 200 | The transaction landed, executed or failed. As on Mainnet, a failed transaction pays its fee, keeps a receipt and cannot be replayed; every other change is undone |
| 400 | Malformed, oversized, unsigned or not a transfer |
| 404 | Unknown account or receipt |
| 409 | Replayed transaction |
| 422 | The fee payer cannot cover the fee |
| 503 | The cluster did not answer |

`POST /leave` answers 200, 400 (bad input, bad signature or unknown blockhash), 404
(wallet not committed), 409 (signature already used), 501 (settlement not configured)
or 503 (cluster problem). The wallet signs `rollup-leave:<wallet>:<blockhash>` with
its key; `rollup_client leave` does it.

Only a 200 charges a fee: 5000 lamports per signature plus any v1 priority fee. An
account left with no lamports is removed.

The server accepts the latest 150 blockhashes it issued, so a client fetches one
right before signing. Every request issues a fresh hash, which gives identical
payments distinct signatures.
