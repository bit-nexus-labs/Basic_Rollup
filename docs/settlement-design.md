# Settlement design: the nine decisions

This document answers the nine decisions of [settlement-challenge.md](settlement-challenge.md): what was chosen, why, and what it costs. The code is in `programs/settlement/` (on-chain program), `settlement_client/` (addresses, instruction builders, parsers), `rollup_core/src/{settle,sequencer,rollupdb,frontend}.rs` (rollup side) and `xtask/` (`cargo xtask settle`, the end-to-end proof).

## The idea in one paragraph

A wallet **deposits** lamports into a **vault**, an account owned by the settlement program at a program-derived address (no private key). The program records the deposit in a **ledger** account for that wallet. The rollup lets a wallet spend only what it deposited; the wallet's own System-owned account is never touched, so it can still pay fees and be used on the cluster. To **leave**, the wallet signs a request to the rollup. The rollup's single sequencer thread sends one cluster transaction in which the program pays the wallet its final rollup balance out of the vault, and only after the cluster confirms does the rollup forget the wallet. Lamports that moved inside the rollup were deposits of other wallets in the same vault, so nobody is paid from thin air.

```
cluster:  wallet --deposit--> [vault PDA]   + [ledger PDA(wallet)] = deposited
rollup:   balance(wallet) = deposited - fees +/- transfers          (in memory)
leave:    wallet signs request -> sequencer sends cluster tx (it pays the fee)
          -> vault --final balance--> wallet, ledger closed (its rent returns to the wallet)
```

## Accounts and instructions

| Account | Owner | Content |
| --- | --- | --- |
| `config`, seeds `["config"]` | settlement program | `sequencer` (32 bytes), `total_committed` (u64): 40 bytes |
| `vault`, seeds `["vault"]` | settlement program | no data; always keeps the rent-exempt minimum for 0 bytes |
| `ledger`, seeds `["ledger", wallet]` | settlement program | `wallet` (32 bytes), `deposited` (u64): 40 bytes |

| Instruction (first byte) | Signed by | Effect |
| --- | --- | --- |
| `0` `initialize(sequencer)` | payer | creates `config` and `vault`; stores the sequencer key; fails if `config` already holds data |
| `1` `deposit(amount)` | wallet | transfers `amount` wallet to vault; creates `ledger`; `total_committed += amount`; fails with `AlreadyDeposited` (error 5) if the wallet has a ledger |
| `2` `leave(amount)` | sequencer | checks the signer equals `config.sequencer` (`NotSequencer`, 6) and `amount <= total_committed` (`OverCommitted`, 7); moves `amount` vault to wallet; closes `ledger` (all its lamports go to the wallet); `total_committed -= amount` |

All account addresses are checked against the derived PDAs; no program id or address is hard-coded anywhere (the program id is a runtime argument).

## The nine decisions

**1. How do lamports get committed? A deposit into a vault, not `Assign`.**
With `Assign` the wallet's own account would become owned by our program. The cluster rules then bite: the account can no longer be a fee payer (fee payers must be System-owned), wallet software no longer sees an ordinary account, and getting it back needs the program to give it away, which works only while its data is empty. With a deposit the wallet keeps its address, its owner and its ability to pay fees, and inside the rollup it is the same address. *Cost:* lamports really leave the wallet on the cluster (that is the point: they cannot be spent twice), the wallet pays one extra transaction and the rent of its `ledger` (returned when it leaves), and all depositors share one vault.

The lecture example for this challenge took the other option: assign the whole wallet to the program (escrow) and hand it back to the System program afterwards. Its own warning is the reason for our choice: after `Assign` the wallet "cannot do anything else on the cluster" until it is returned. We kept that route closed on purpose and note the trade: delegation keeps one account per wallet and no shared pool, but locks the wallet; a deposit frees the wallet but needs a pool and the extra accounting of decisions 3 to 5.

**2. What does the rollup accept? Only wallets that have a ledger.**
In settlement mode the rollup no longer copies a wallet's cluster balance (the old `pull`). On first use of an address it reads that wallet's ledger and starts the rollup balance at `deposited`. A transaction that pays from or to an address without a ledger is refused with HTTP 400, `account <address> has not committed lamports to settlement`, and **no fee is charged**; `GET /get_account` answers 404 for it. The System program is exempt. *Cost:* a receiver must deposit something first. A deposit smaller than the rollup's own rent-exempt minimum for an empty account (650240 lamports) is not usable in the rollup.

**3. Balances or differences? One final balance, because the wallet's own account is not used.**
Committed lamports sit in the vault. The wallet's own cluster account is not read or written by the rollup, so lamports that arrive there while the wallet is in the rollup never mix with rollup state, and no merging of differences is needed. `leave` pays one number: the wallet's final rollup balance. *Cost:* the program cannot compare `amount` with `deposited` (a receiver may leave with more than it deposited, a payer with less); its last guard is the pool total (decision 5).

**4. Between whom are lamports moved? Each wallet and a shared pool.**
Wallets never pay each other on the cluster. A leaver is paid from the vault and every other wallet stays. This is what lets the receiver leave first while the payer stays. *Cost:* the vault is only as safe as the numbers the sequencer reports (decision 6).

**5. Where do an early leaver's lamports come from? From the vault, not from the sequencer.**
The rollup keeps one invariant: the sum of rollup balances plus the fees paid inside the rollup equals the deposits not yet withdrawn, and that is what the vault holds (plus its rent minimum). So the receiver who leaves first is paid with the payer's deposit; the sequencer lends nothing. The program enforces `amount <= total_committed` as a last guard. Rollup fees (5000 lamports per signature) stay in the vault; they are counted in `total_committed` and are **not claimable** in this design (a stated limit). In `cargo xtask settle` the vault ends with exactly its rent minimum plus 5000.

**6. Who is allowed to settle? Only the sequencer key stored in `config`.**
`initialize` stores the sequencer's public key. `leave` must be signed by it and the program compares the signer with `config.sequencer`. A `leave` signed by anyone else fails with `NotSequencer`; `cargo xtask settle` shows it. The rollup additionally requires the wallet's own signature on a leave request: the message is `rollup-leave:<wallet>:<blockhash>`, the blockhash must be one the rollup issued recently, and each signature works once (`409` on replay). *Cost:* proofs are out of scope, so a stolen or malicious sequencer key can drain the vault. Also: the first `initialize` wins the sequencer role (nobody can re-initialize, but nothing stops a stranger from initializing first on a fresh deployment).

**7. Who pays the cluster fees?**
`deposit`: the wallet (System-owned and funded). `leave`: the sequencer is the fee payer, so a wallet that spent everything in the rollup and holds nothing on the cluster can still leave. `initialize`: the payer, in the demo the sequencer. This works because decision 1 keeps wallets System-owned. With `Assign` a committed wallet could not pay a fee at any moment.

**8. How does a wallet leave? Signed request, freeze, cluster transaction, forget.**
(a) The wallet signs the request and sends `POST /leave`. (b) The sequencer thread checks, in this order: settlement configured, blockhash recent, signature valid, signature not used, wallet committed. (c) It reads the wallet's final balance (the rollup is single-threaded, so no other transaction can change it: this is the freeze; the whole rollup waits while the cluster confirms, which is a limit). (d) It sends `leave(balance)` with `confirmed` commitment. (e) Only after confirmation it removes the wallet and remembers the signature. If the cluster transaction fails and the ledger still exists, the wallet stays and the answer is `503`; the request can be retried with a fresh blockhash. *Must be true at the moment of sending:* the wallet's rollup balance is final.

**9. If the sequencer disappears or restarts?**
Rollup state is in memory. What is guaranteed: deposited lamports sit in the vault and cannot move without the sequencer key. After a restart the rollup rebuilds each wallet from its ledger, i.e. from the deposit, so **transfers made inside the rollup before the restart are lost**. Not guaranteed: recovery of rollup transfers, withdrawal without the sequencer, protection against a malicious sequencer. See the limits below.

## How it differs from MagicBlock's delegation program

Read after the first design, as the challenge asks. **Honest scope of this comparison:** I read MagicBlock's public documentation (Ephemeral Rollups introduction and the Rust program guide) and the docs.rs summary of `magicblock-labs/delegation-program` (instruction and state names, file layout). I did **not** read the Rust source of the program line by line, so statements about its internals below come from those pages.

What MagicBlock does: a program **delegates the account itself**. The account's owner changes to the Delegation Program and an ER validator is named; a delegation record (authority, owner, commit frequency) and metadata (seeds, limits) are stored; the validator clones the account into the ephemeral rollup on first use; state is **committed back repeatedly** (commit, finalize); **undelegation** commits the final state and the owner reverts to the original program through a callback. Finality of the committed state rests on a fraud-proof mechanism run by a security committee.

| Decision | MagicBlock (from the docs) | This project |
| --- | --- | --- |
| 1 commit | the account itself is assigned to the delegation program (`Assign`-like) | lamports are deposited into a program-owned vault; the wallet keeps its account |
| 2 acceptance | the ER clones a delegated account on first use | only wallets with a ledger; refused with no fee otherwise |
| 3 what is settled | the full state of the delegated account, committed repeatedly | one final balance per wallet, at leave only |
| 4 between whom | per account, no shared pool | each wallet and one shared vault |
| 5 early leaver | not a problem: each delegated account holds its own lamports | paid from the vault; invariant plus `total_committed` guard |
| 6 who settles | the named validator; authority stored in the delegation record | one sequencer key in `config`; trusted |
| 7 fees | a fee payer or an escrow can cover commit fees; rent top-ups come from a delegated payer | wallet pays deposit, sequencer pays leave |
| 9 restart | periodic commits keep state on the base layer | no commits; inside-rollup transfers are lost |

The main trade: delegation keeps real account state on the cluster during the session and pays for it with a trusted-or-proven validator and a locked account (the owner is not System, so it cannot pay fees meanwhile, which is why their docs talk about fee payers and escrows). Our pool design keeps wallets free and needs no state commits, but gives up recovery of rollup activity and trusts the sequencer completely.

## Limits (honest list)

- The sequencer is trusted; there are no proofs. A stolen sequencer key drains the vault.
- The first `initialize` on a deployment decides who the sequencer is.
- Rollup fees (5000 per signature) stay in the vault; they cannot be claimed.
- Rollup state is in memory. After a restart the rollup rebuilds balances from the ledgers and every transfer made inside the rollup before is lost. The set of used leave signatures is also in memory (the 150-blockhash window is lost too, so old requests are refused anyway).
- One deposit per wallet until it leaves (`AlreadyDeposited`). A deposit below 650240 lamports cannot be used inside the rollup.
- While a `leave` waits for cluster confirmation the single sequencer thread is blocked: the whole rollup waits (the freeze is a side effect of one thread, not a lock per wallet).
- If the cluster accepts a `leave` but the confirmation is lost and the ledger is already gone, the rollup treats the leave as done (answer with an empty `cluster_signature`). If the transaction lands later than the rollup's check, the wallet could be paid on the cluster while the rollup still shows its balance; a retry then fails. This race is not closed.
- Rollup transactions are native SOL transfers only. The vault is checked against the System program rules of the local test validator; nothing was run on a public cluster.
- Size: the whole solution is about 1770 added lines (counted against base commit `3815ce1`, lock files and the two docs excluded): the program 301, the client crate 132, the rollup server and client changes about 610, `cargo xtask settle` about 720 (it is formatted normally, not packed into long lines), and a few tests. The `settle` scenario was refactored into small steps after the first version, which made it easier to read but not shorter. That is well over the challenge's guideline of under a thousand lines; nothing was cut to hit the number.

## What `cargo xtask settle` proves

On a local validator, with the real server and client, it checks owners and exact lamports after every step: initialize; deposits of 2 SOL (A) and 1 SOL (B); vault = rent minimum + 3 SOL, `total_committed` = 3 SOL; A pays B 500000000 (A 1499995000, B 1500000000); refusals (fresh recipient, uncommitted sender, wrong signature, unknown blockhash, `leave` not from the sequencer, second deposit); B leaves first (paid 1500000000, vault down by that amount, ledger closed and its rent returned, rollup 404, repeat leave 404, replay 409); A leaves later (1499995000); the vault ends with rent minimum + 5000; `total_committed` = 5000. `cargo xtask verify` still passes, and the rollup still runs from a genesis file when settlement is not configured.
