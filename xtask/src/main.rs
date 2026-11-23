//! Project tasks, run as `cargo xtask <task>`.

use std::{
    env, fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use solana_keypair::{read_keypair_file, write_keypair_file, Keypair};
use solana_pubkey::Pubkey;
use solana_rpc_client::{api::config::CommitmentConfig, rpc_client::RpcClient};
use solana_signer::Signer;
use solana_transaction::Transaction;

const USAGE: &str = "\
Usage: cargo xtask <task>

Tasks:
  demo     Create the demo keys and genesis in demo/ and print how to run the rollup
  verify   Format, lint and test the workspace, then run the smoke test
  smoke    Run the real server and client on loopback and check the resulting balances
  settle   Run the full settlement scenario on a real local validator
  mainnet  Compare the rollup's Mainnet snapshot with the live cluster (needs network)
  live     Pull an account from ROLLUP_TEST_CLUSTER_URL (default devnet; needs network)";

const GENESIS_LAMPORTS: u64 = 10_000_000_000;
const TRANSFER_LAMPORTS: u64 = 100_000_000;
const TRANSFERS: u64 = 2;
const FEE_LAMPORTS: u64 = 5_000;

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("demo") => demo(),
        Some("verify") => verify(),
        Some("smoke") => smoke(),
        Some("settle") => settle(),
        Some("mainnet") => ignored_test("mainnet_snapshot_is_current"),
        Some("live") => ignored_test("live_cluster_account_matches_the_rollup"),
        _ => bail!("{USAGE}"),
    }
}

fn verify() -> Result<()> {
    cargo(&["fmt", "--all"])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo(&["test", "--workspace", "--locked"])?;

    smoke()
}

/// Creates the demo keys and genesis, then prints how to run the rollup by hand.
fn demo() -> Result<()> {
    let (payer, _) = demo_keypair("payer")?;
    let recipient = demo_keypair("recipient")?.0.pubkey();
    demo_genesis(&payer)?;

    println!(
        "The demo keys and genesis are in demo/, which git ignores.\n\n\
         Start the rollup:\n  \
         ROLLUP_GENESIS=demo/genesis.json cargo run -p rollup_core --locked\n\n\
         In a second terminal, pay the recipient and read its balance:\n  \
         cargo run -p rollup_client --locked -- demo/payer.json {recipient} {TRANSFER_LAMPORTS}\n  \
         curl http://127.0.0.1:8080/get_account/{recipient}"
    );
    Ok(())
}

/// The keypair in `demo/<name>.json`. It is created the first time it is asked for
/// and reused by every later run. Git ignores the folder, so the keys stay local.
fn demo_keypair(name: &str) -> Result<(Keypair, PathBuf)> {
    let directory = workspace_root().join("demo");
    fs::create_dir_all(&directory)?;
    let file = directory.join(format!("{name}.json"));

    let keypair = if file.exists() {
        read_keypair_file(&file)
            .map_err(|error| anyhow::anyhow!("reading {}: {error}", file.display()))?
    } else {
        let keypair = Keypair::new();
        write_keypair_file(&keypair, &file)
            .map_err(|error| anyhow::anyhow!("writing {}: {error}", file.display()))?;
        keypair
    };
    Ok((keypair, file))
}

/// Writes `demo/genesis.json`, which funds the demo payer.
fn demo_genesis(payer: &Keypair) -> Result<PathBuf> {
    let file = workspace_root().join("demo").join("genesis.json");
    let genesis = json!({"accounts": [
        {"address": payer.pubkey().to_string(), "lamports": GENESIS_LAMPORTS}
    ]});
    fs::write(&file, serde_json::to_vec_pretty(&genesis)?)?;
    Ok(file)
}

/// Runs one of the tests that plain `cargo test` skips because it needs the network.
fn ignored_test(name: &str) -> Result<()> {
    cargo(&[
        "test",
        "--package",
        "rollup_core",
        "--locked",
        name,
        "--",
        "--ignored",
        "--nocapture",
    ])
}

/// Sends two identical transfers with the real client to the real server, then checks
/// the receipts, the exact balances and a clean shutdown on SIGTERM.
fn smoke() -> Result<()> {
    cargo(&["build", "--workspace", "--locked"])?;

    let binaries = target_directory().join("debug");
    let artifacts = target_directory().join("verification").join("cli");
    fs::create_dir_all(&artifacts)?;

    // Phase 1, fixtures: the demo keys, created on the first run and reused after it.
    let (payer, payer_file) = demo_keypair("payer")?;
    let recipient = demo_keypair("recipient")?.0.pubkey();
    let genesis_file = demo_genesis(&payer)?;

    // Phase 2, server: start the rollup on a free loopback port and wait until it answers.
    let address = TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let url = format!("http://{address}");
    let log = fs::File::create(artifacts.join("server.log"))?;
    let mut server = Server(
        Command::new(binaries.join("rollup_core"))
            .env("ROLLUP_GENESIS", &genesis_file)
            .env("ROLLUP_BIND", address.to_string())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .context("starting rollup_core")?,
    );
    let http = Client::builder().timeout(Duration::from_secs(5)).build()?;
    server.wait_until_ready(&http, &url)?;

    // Phase 3, transfers: the same payment twice must give two distinct transactions.
    let mut receipts = Vec::new();
    for _ in 0..TRANSFERS {
        let output = Command::new(binaries.join("rollup_client"))
            .arg(&payer_file)
            .arg(recipient.to_string())
            .arg(TRANSFER_LAMPORTS.to_string())
            .arg(&url)
            .output()
            .context("running rollup_client")?;
        ensure!(
            output.status.success(),
            "rollup_client failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let receipt: Value = serde_json::from_slice(&output.stdout)?;
        ensure!(receipt["status"] == "executed", "receipt: {receipt}");
        ensure!(receipt["version"] == 1, "client must send v1: {receipt}");
        ensure!(receipt["fee"] == FEE_LAMPORTS, "receipt: {receipt}");
        ensure!(receipt["settlement"] == "not_configured", "{receipt}");
        receipts.push(receipt);
    }
    ensure!(
        receipts[0]["signature"] != receipts[1]["signature"],
        "identical payments must still be distinct transactions"
    );

    // Phase 4, state: the balances must match the transfers and fees exactly.
    let balance = |address: String| -> Result<u64> {
        let account: Value = http
            .get(format!("{url}/get_account/{address}"))
            .send()?
            .error_for_status()?
            .json()?;
        account["lamports"].as_u64().context("lamport balance")
    };
    let payer_balance = balance(payer.pubkey().to_string())?;
    let recipient_balance = balance(recipient.to_string())?;
    let spent = TRANSFERS * (TRANSFER_LAMPORTS + FEE_LAMPORTS);
    ensure!(
        payer_balance == GENESIS_LAMPORTS - spent,
        "payer balance is {payer_balance}"
    );
    ensure!(
        recipient_balance == TRANSFERS * TRANSFER_LAMPORTS,
        "recipient balance is {recipient_balance}"
    );

    // Phase 5, shutdown: SIGTERM must end the server cleanly.
    let exit_code = server.terminate()?;
    ensure!(exit_code == 0, "SIGTERM shutdown exited with {exit_code}");

    let report = json!({
        "command": "cargo xtask smoke",
        "inputs": "The demo keypairs; demo/genesis.json; two identical v1 transfers from rollup_client",
        "payer": payer.pubkey().to_string(),
        "recipient": recipient.to_string(),
        "receipts": receipts,
        "final_balances": {"payer": payer_balance, "recipient": recipient_balance},
        "sigterm_exit_code": exit_code,
        "scope": "Real client, HTTP server and SVM on loopback; no validator, RPC network or L1 settlement",
        "result": "passed",
    });
    let report_file = artifacts.join("result.json");
    fs::write(&report_file, serde_json::to_vec_pretty(&report)?)?;

    println!(
        "Smoke test passed: two v1 transfers, exact balances, clean shutdown.\nReport: {}",
        report_file.display()
    );
    Ok(())
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
}

fn target_directory() -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn cargo(arguments: &[&str]) -> Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    println!("$ cargo {}", arguments.join(" "));

    let status = Command::new(cargo)
        .args(arguments)
        .current_dir(workspace_root())
        .status()
        .context("running cargo")?;
    ensure!(status.success(), "cargo {} failed", arguments.join(" "));
    Ok(())
}

/// The running rollup server; it is killed if a check fails before `terminate`.
struct Server(Child);

impl Server {
    fn wait_until_ready(&mut self, http: &Client, url: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.0.try_wait()? {
                bail!("rollup_core exited before it was ready: {status}; see server.log");
            }
            if http
                .get(url)
                .send()
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "rollup_core readiness timed out");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn terminate(&mut self) -> Result<i32> {
        let signalled = Command::new("kill")
            .arg("-TERM")
            .arg(self.0.id().to_string())
            .status()
            .context("sending SIGTERM")?;
        ensure!(signalled.success(), "could not signal rollup_core");

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait()? {
                return status.code().context("rollup_core was killed by a signal");
            }
            ensure!(Instant::now() < deadline, "rollup_core ignored SIGTERM");
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn check_eq<T: std::fmt::Debug + PartialEq>(actual: T, expected: T, desc: &str) -> Result<()> {
    ensure!(
        actual == expected,
        "{desc}: expected {expected:?}, got {actual:?}"
    );
    Ok(())
}

fn settle() -> Result<()> {
    let settle_dir = target_directory().join("settle");
    let mut guard = SettleGuard {
        dir: settle_dir.clone(),
        validator: None,
        rollup: None,
    };
    if let Err(e) = run_settle(&mut guard) {
        eprintln!("settle failed: {e:#}");
        print_log_tail(&settle_dir.join("rollup.log"), "rollup.log");
        print_log_tail(&settle_dir.join("validator.log"), "validator.log");
        guard.kill();
        let _ = fs::remove_dir_all(&settle_dir);
        std::process::exit(1);
    }
    guard.kill();
    let _ = fs::remove_dir_all(&settle_dir);
    println!("settle PASSED");
    Ok(())
}

fn print_log_tail(path: &Path, name: &str) {
    if let Ok(c) = fs::read_to_string(path) {
        let lines: Vec<&str> = c.lines().collect();
        eprintln!("--- last 30 lines of {name} ---");
        for line in &lines[lines.len().saturating_sub(30)..] {
            eprintln!("{line}");
        }
    }
}

struct SettleGuard {
    dir: PathBuf,
    validator: Option<Child>,
    rollup: Option<Child>,
}

impl SettleGuard {
    fn kill(&mut self) {
        for mut c in [self.rollup.take(), self.validator.take()]
            .into_iter()
            .flatten()
        {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for SettleGuard {
    fn drop(&mut self) {
        self.kill();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn pstr(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))
}

fn run_settle(guard: &mut SettleGuard) -> Result<()> {
    ensure!(
        Command::new("solana-test-validator")
            .arg("--version")
            .output()
            .is_ok(),
        "solana-test-validator is not on PATH"
    );
    ensure!(
        Command::new("cargo-build-sbf")
            .arg("--version")
            .output()
            .is_ok(),
        "cargo-build-sbf is not on PATH"
    );
    ensure!(
        TcpListener::bind("127.0.0.1:8899").is_ok(),
        "port 8899 is busy"
    );
    ensure!(
        TcpListener::bind("127.0.0.1:8080").is_ok(),
        "port 8080 is busy"
    );
    println!("ok: environment and tools checked");

    let status = Command::new("cargo-build-sbf")
        .current_dir(workspace_root().join("programs/settlement"))
        .status()
        .context("cargo-build-sbf")?;
    ensure!(status.success(), "building settlement program failed");
    cargo(&["build", "--workspace", "--locked"])?;
    println!("ok: program and workspace built");

    if guard.dir.exists() {
        fs::remove_dir_all(&guard.dir)?;
    }
    fs::create_dir_all(&guard.dir)?;

    let make_key = |name: &str| -> Result<(Keypair, PathBuf)> {
        let (kp, path) = (Keypair::new(), guard.dir.join(format!("{name}.json")));
        write_keypair_file(&kp, &path).map_err(|e| anyhow!("{e}"))?;
        Ok((kp, path))
    };
    let (sequencer_key, seq_file) = make_key("sequencer")?;
    let (wallet_a_key, a_file) = make_key("wallet-a")?;
    let (wallet_b_key, b_file) = make_key("wallet-b")?;
    let (fresh_key, fresh_file) = make_key("fresh")?;
    let (program_key, prog_file) = make_key("program")?;
    drop(prog_file);

    let (seq_pubkey, a_pubkey, b_pubkey, fresh_pubkey, program_id) = (
        sequencer_key.pubkey(),
        wallet_a_key.pubkey(),
        wallet_b_key.pubkey(),
        fresh_key.pubkey(),
        program_key.pubkey(),
    );

    let val_log = fs::File::create(guard.dir.join("validator.log"))?;
    let so_path = workspace_root().join("programs/settlement/target/deploy/settlement.so");
    let ledger_path = guard.dir.join("ledger");
    let validator = Command::new("solana-test-validator")
        .args([
            "--reset",
            "--ledger",
            pstr(&ledger_path)?,
            "--upgradeable-program",
            &program_id.to_string(),
            pstr(&so_path)?,
            "none",
        ])
        .stdout(val_log.try_clone()?)
        .stderr(val_log)
        .spawn()
        .context("starting solana-test-validator")?;
    guard.validator = Some(validator);

    let rpc = RpcClient::new_with_timeout_and_commitment(
        "http://127.0.0.1:8899",
        Duration::from_secs(5),
        CommitmentConfig::confirmed(),
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut val_ready = false;
    while Instant::now() < deadline {
        let val = guard.validator.as_mut().context("missing validator")?;
        if let Some(status) = val.try_wait()? {
            bail!("solana-test-validator exited early with {status}");
        }
        if rpc.get_version().is_ok() {
            val_ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    ensure!(
        val_ready,
        "solana-test-validator failed to become ready in 90s"
    );
    println!("ok: validator started and ready");

    let airdrop = |pk: &Pubkey, lamports: u64| -> Result<()> {
        rpc.request_airdrop(pk, lamports)
            .context("airdrop request")?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(bal) = rpc.get_balance(pk) {
                if bal >= lamports {
                    return Ok(());
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
        bail!("airdrop to {pk} timed out");
    };
    for pk in [&seq_pubkey, &a_pubkey, &b_pubkey] {
        airdrop(pk, 5_000_000_000)?;
    }

    let binaries = target_directory().join("debug");
    let client_bin = binaries.join("rollup_client");
    let run_client = |args: &[&str]| -> Result<String> {
        let output = Command::new(&client_bin)
            .args(args)
            .output()
            .with_context(|| format!("running rollup_client {}", args.join(" ")))?;
        ensure!(
            output.status.success(),
            "rollup_client {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    };

    let prog_str = program_id.to_string();
    for (cmd, file, amt) in [
        ("initialize", &seq_file, ""),
        ("deposit", &a_file, "2000000000"),
        ("deposit", &b_file, "1000000000"),
    ] {
        let mut args = vec![cmd, pstr(file)?, &prog_str];
        if !amt.is_empty() {
            args.push(amt);
        }
        args.push("http://127.0.0.1:8899");
        run_client(&args)?;
    }
    println!("ok: airdrops and initialization completed");

    let rollup_log = fs::File::create(guard.dir.join("rollup.log"))?;
    let rollup = Command::new(binaries.join("rollup_core"))
        .env("ROLLUP_CLUSTER_URL", "http://127.0.0.1:8899")
        .env("ROLLUP_SETTLEMENT_PROGRAM", program_id.to_string())
        .env("ROLLUP_SEQUENCER_KEYPAIR", &seq_file)
        .env("ROLLUP_BIND", "127.0.0.1:8080")
        .stdout(rollup_log.try_clone()?)
        .stderr(rollup_log)
        .spawn()
        .context("starting rollup_core")?;
    guard.rollup = Some(rollup);

    let http = Client::builder().timeout(Duration::from_secs(5)).build()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut rollup_ready = false;
    while Instant::now() < deadline {
        let roll = guard.rollup.as_mut().context("missing rollup")?;
        if let Some(status) = roll.try_wait()? {
            bail!("rollup_core exited early with {status}");
        }
        if let Ok(res) = http.get("http://127.0.0.1:8080/").send() {
            if res.status().is_success() && res.json::<Value>()?["settlement"] == "configured" {
                rollup_ready = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    ensure!(rollup_ready, "rollup_core failed to become ready in 30s");
    println!("ok: rollup started in settlement mode");

    let rollup_balance = |pk: &Pubkey| -> Result<Option<u64>> {
        let res = http
            .get(format!("http://127.0.0.1:8080/get_account/{pk}"))
            .send()?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(res.error_for_status()?.json::<Value>()?["lamports"].as_u64())
    };

    check_eq(
        rollup_balance(&a_pubkey)?,
        Some(2_000_000_000),
        "initial rollup balance A",
    )?;
    check_eq(
        rollup_balance(&b_pubkey)?,
        Some(1_000_000_000),
        "initial rollup balance B",
    )?;

    let vault_pda = settlement_client::vault_address(&program_id);
    let rent_min_0 = rpc.get_minimum_balance_for_rent_exemption(0)?;
    check_eq(
        rpc.get_balance(&vault_pda)?,
        rent_min_0 + 3_000_000_000,
        "initial cluster vault balance",
    )?;

    let config_pda = settlement_client::config_address(&program_id);
    let config_acc = rpc.get_account(&config_pda)?;
    let (_, total_committed) =
        settlement_client::parse_config(&config_acc.data).context("parsing config")?;
    check_eq(total_committed, 3_000_000_000, "initial total_committed")?;
    println!("ok: initial balances verified");

    run_client(&[
        pstr(&a_file)?,
        &b_pubkey.to_string(),
        "500000000",
        "http://127.0.0.1:8080",
    ])?;
    check_eq(
        rollup_balance(&a_pubkey)?,
        Some(1_499_995_000),
        "balance A after transfer",
    )?;
    check_eq(
        rollup_balance(&b_pubkey)?,
        Some(1_500_000_000),
        "balance B after transfer",
    )?;
    println!("ok: transfer A to B verified");

    let check_refusal = |args: &[&str], desc: &str| -> Result<()> {
        let out = Command::new(&client_bin).args(args).output()?;
        ensure!(!out.status.success(), "{desc} must fail");
        let err = String::from_utf8_lossy(&out.stderr);
        ensure!(
            err.contains("has not committed lamports to settlement"),
            "{desc}: {err}"
        );
        Ok(())
    };
    check_refusal(
        &[
            pstr(&a_file)?,
            &fresh_pubkey.to_string(),
            "100000000",
            "http://127.0.0.1:8080",
        ],
        "fresh recipient",
    )?;
    check_eq(
        rollup_balance(&a_pubkey)?,
        Some(1_499_995_000),
        "A balance unchanged after unfunded transfer refusal",
    )?;
    check_refusal(
        &[
            pstr(&fresh_file)?,
            &b_pubkey.to_string(),
            "100000000",
            "http://127.0.0.1:8080",
        ],
        "uncommitted sender",
    )?;

    let check_leave_status =
        |wallet: &Pubkey, bh: &str, sig: &str, expected: u16, desc: &str| -> Result<()> {
            let res = http
                .post("http://127.0.0.1:8080/leave")
                .json(&json!({"wallet": wallet.to_string(), "blockhash": bh, "signature": sig}))
                .send()?;
            check_eq(res.status().as_u16(), expected, desc)
        };

    let bh_val: Value = http
        .get("http://127.0.0.1:8080/get_latest_blockhash")
        .send()?
        .json()?;
    let blockhash = bh_val["blockhash"].as_str().context("missing blockhash")?;
    let bad_sig = wallet_a_key.sign_message(&settlement_client::leave_request_message(
        &b_pubkey, blockhash,
    ));
    check_leave_status(
        &b_pubkey,
        blockhash,
        &bad_sig.to_string(),
        400,
        "leave with mismatched signature must be 400",
    )?;

    let fake_bh = solana_hash::Hash::new_unique().to_string();
    let fake_sig = wallet_b_key.sign_message(&settlement_client::leave_request_message(
        &b_pubkey, &fake_bh,
    ));
    check_leave_status(
        &b_pubkey,
        &fake_bh,
        &fake_sig.to_string(),
        400,
        "leave with unknown blockhash must be 400",
    )?;
    println!("ok: refusals verified");

    let seq_cluster_before = rpc.get_balance(&seq_pubkey)?;
    let vault_bal_before = rpc.get_balance(&vault_pda)?;
    let cfg_acc = rpc.get_account(&config_pda)?;
    let (_, committed_before) =
        settlement_client::parse_config(&cfg_acc.data).context("parsing config")?;

    let latest_bh = rpc.get_latest_blockhash()?;
    let fake_leave_ix = settlement_client::leave_instruction(&program_id, &a_pubkey, &a_pubkey, 0);
    let fake_leave_tx = Transaction::new_signed_with_payer(
        &[fake_leave_ix],
        Some(&a_pubkey),
        &[&wallet_a_key],
        latest_bh,
    );
    let err = rpc
        .send_and_confirm_transaction(&fake_leave_tx)
        .err()
        .context("leave not from sequencer must fail")?;
    let err_str = err.to_string();
    ensure!(
        err_str.contains("custom program error: 0x6"),
        "unexpected error for NotSequencer: {err_str}"
    );
    check_eq(
        rpc.get_balance(&vault_pda)?,
        vault_bal_before,
        "vault balance unchanged after NotSequencer",
    )?;
    let cfg_acc = rpc.get_account(&config_pda)?;
    let (_, comm) = settlement_client::parse_config(&cfg_acc.data).context("parsing config")?;
    check_eq(
        comm,
        committed_before,
        "total_committed unchanged after NotSequencer",
    )?;
    println!("ok: leave not from the sequencer refused (NotSequencer)");

    let out = Command::new(&client_bin)
        .args([
            "deposit",
            pstr(&a_file)?,
            &prog_str,
            "1000",
            "http://127.0.0.1:8899",
        ])
        .output()
        .context("running second deposit")?;
    ensure!(!out.status.success(), "second deposit must fail");
    let err_str = String::from_utf8_lossy(&out.stderr);
    ensure!(
        err_str.contains("0x5"),
        "unexpected error for AlreadyDeposited: {err_str}"
    );
    check_eq(
        rpc.get_balance(&vault_pda)?,
        vault_bal_before,
        "vault balance unchanged after AlreadyDeposited",
    )?;
    let cfg_acc = rpc.get_account(&config_pda)?;
    let (_, comm) = settlement_client::parse_config(&cfg_acc.data).context("parsing config")?;
    check_eq(
        comm,
        committed_before,
        "total_committed unchanged after AlreadyDeposited",
    )?;
    println!("ok: second deposit refused (AlreadyDeposited)");

    let ledger_b_pda = settlement_client::ledger_address(&program_id, &b_pubkey);
    let ledger_rent = rpc.get_minimum_balance_for_rent_exemption(40)?;
    let ledger_b_acc = rpc.get_account(&ledger_b_pda)?;
    check_eq(
        ledger_b_acc.lamports,
        ledger_rent,
        "ledger B balance before leave",
    )?;
    let b_cluster_before = rpc.get_balance(&b_pubkey)?;
    let vault_before_b = rpc.get_balance(&vault_pda)?;

    let bh_val: Value = http
        .get("http://127.0.0.1:8080/get_latest_blockhash")
        .send()?
        .json()?;
    let bh_b = bh_val["blockhash"].as_str().context("missing blockhash")?;
    let sig_b =
        wallet_b_key.sign_message(&settlement_client::leave_request_message(&b_pubkey, bh_b));
    let leave_b_body = json!({
        "wallet": b_pubkey.to_string(),
        "blockhash": bh_b,
        "signature": sig_b.to_string()
    });

    let res = http
        .post("http://127.0.0.1:8080/leave")
        .json(&leave_b_body)
        .send()?;
    check_eq(res.status().as_u16(), 200, "leave B status")?;
    let receipt_b: Value = res.json()?;
    check_eq(
        receipt_b["lamports"].as_u64(),
        Some(1_500_000_000),
        "leave B lamports",
    )?;

    let b_cluster_after = rpc.get_balance(&b_pubkey)?;
    check_eq(
        b_cluster_after,
        b_cluster_before + 1_500_000_000 + ledger_rent,
        "cluster balance B after leave",
    )?;

    let vault_after_b = rpc.get_balance(&vault_pda)?;
    check_eq(
        vault_after_b,
        vault_before_b - 1_500_000_000,
        "vault balance after B leave",
    )?;

    ensure!(
        rpc.get_account(&ledger_b_pda).is_err(),
        "ledger B must be gone"
    );
    check_eq(
        rollup_balance(&b_pubkey)?,
        None,
        "rollup balance B must be 404",
    )?;

    let bh2_val: Value = http
        .get("http://127.0.0.1:8080/get_latest_blockhash")
        .send()?
        .json()?;
    let bh2 = bh2_val["blockhash"].as_str().context("missing blockhash")?;
    let sig2 = wallet_b_key.sign_message(&settlement_client::leave_request_message(&b_pubkey, bh2));
    check_leave_status(
        &b_pubkey,
        bh2,
        &sig2.to_string(),
        404,
        "second leave B must be 404",
    )?;
    check_leave_status(
        &b_pubkey,
        bh_b,
        &sig_b.to_string(),
        409,
        "replaying leave B must be 409",
    )?;
    println!("ok: wallet B leave verified");

    let ledger_a_pda = settlement_client::ledger_address(&program_id, &a_pubkey);
    let ledger_a_acc = rpc.get_account(&ledger_a_pda)?;
    check_eq(
        ledger_a_acc.lamports,
        ledger_rent,
        "ledger A balance before leave",
    )?;
    let a_cluster_before = rpc.get_balance(&a_pubkey)?;

    let out = run_client(&["leave", pstr(&a_file)?, "http://127.0.0.1:8080"])?;
    let receipt_a: Value = serde_json::from_str(&out)?;
    check_eq(
        receipt_a["lamports"].as_u64(),
        Some(1_499_995_000),
        "leave A lamports",
    )?;

    let a_cluster_after = rpc.get_balance(&a_pubkey)?;
    check_eq(
        a_cluster_after,
        a_cluster_before + 1_499_995_000 + ledger_rent,
        "cluster balance A after leave",
    )?;

    ensure!(
        rpc.get_account(&ledger_a_pda).is_err(),
        "ledger A must be gone"
    );
    check_eq(
        rollup_balance(&a_pubkey)?,
        None,
        "rollup balance A must be 404",
    )?;

    let vault_final = rpc.get_balance(&vault_pda)?;
    check_eq(
        vault_final,
        rent_min_0 + 5_000,
        "vault balance after A leave",
    )?;

    let config_acc_final = rpc.get_account(&config_pda)?;
    let (_, total_committed_final) =
        settlement_client::parse_config(&config_acc_final.data).context("parsing config")?;
    check_eq(
        total_committed_final,
        5_000,
        "total_committed after A leave",
    )?;

    check_eq(
        rpc.get_balance(&seq_pubkey)?,
        seq_cluster_before - 10_000,
        "sequencer cluster balance dropped by 2 cluster fees",
    )?;
    println!("ok: wallet A leave verified");

    Ok(())
}
