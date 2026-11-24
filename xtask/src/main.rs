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

fn sign_leave(key: &Keypair, wallet: &Pubkey, bh: &str) -> String {
    key.sign_message(&settlement_client::leave_request_message(wallet, bh))
        .to_string()
}

struct SettleContext {
    rpc: RpcClient,
    http: Client,
    program_id: Pubkey,
    vault_pda: Pubkey,
    config_pda: Pubkey,
    client_bin: PathBuf,
}

impl SettleContext {
    fn new(rpc: RpcClient, http: Client, program_id: Pubkey, client_bin: PathBuf) -> Self {
        let vault_pda = settlement_client::vault_address(&program_id);
        let config_pda = settlement_client::config_address(&program_id);
        Self {
            rpc,
            http,
            program_id,
            vault_pda,
            config_pda,
            client_bin,
        }
    }

    fn airdrop(&self, pk: &Pubkey, lamports: u64) -> Result<()> {
        self.rpc
            .request_airdrop(pk, lamports)
            .context("airdrop request")?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(bal) = self.rpc.get_balance(pk) {
                if bal >= lamports {
                    return Ok(());
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
        bail!("airdrop to {pk} timed out");
    }

    fn client(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.client_bin)
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
    }

    fn client_refusal(&self, args: &[&str], expected_err: &str, desc: &str) -> Result<()> {
        let out = Command::new(&self.client_bin).args(args).output()?;
        ensure!(!out.status.success(), "{desc} must fail");
        let err = String::from_utf8_lossy(&out.stderr);
        ensure!(err.contains(expected_err), "{desc}: {err}");
        Ok(())
    }

    fn rollup_balance(&self, pk: &Pubkey) -> Result<Option<u64>> {
        let res = self
            .http
            .get(format!("http://127.0.0.1:8080/get_account/{pk}"))
            .send()?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(res.error_for_status()?.json::<Value>()?["lamports"].as_u64())
    }

    fn blockhash(&self) -> Result<String> {
        let v: Value = self
            .http
            .get("http://127.0.0.1:8080/get_latest_blockhash")
            .send()?
            .json()?;
        v["blockhash"]
            .as_str()
            .map(String::from)
            .context("missing blockhash")
    }

    fn post_leave(
        &self,
        wallet: &Pubkey,
        bh: &str,
        sig: &str,
    ) -> Result<reqwest::blocking::Response> {
        self.http
            .post("http://127.0.0.1:8080/leave")
            .json(&json!({"wallet": wallet.to_string(), "blockhash": bh, "signature": sig}))
            .send()
            .context("post leave")
    }

    fn check_leave_status(
        &self,
        wallet: &Pubkey,
        bh: &str,
        sig: &str,
        expected: u16,
        desc: &str,
    ) -> Result<()> {
        let res = self.post_leave(wallet, bh, sig)?;
        check_eq(res.status().as_u16(), expected, desc)
    }

    fn vault_balance(&self) -> Result<u64> {
        Ok(self.rpc.get_balance(&self.vault_pda)?)
    }

    fn total_committed(&self) -> Result<u64> {
        let acc = self.rpc.get_account(&self.config_pda)?;
        let (_, comm) = settlement_client::parse_config(&acc.data).context("parsing config")?;
        Ok(comm)
    }

    fn check_vault_and_committed(
        &self,
        expected_vault: u64,
        expected_committed: u64,
        suffix: &str,
    ) -> Result<()> {
        check_eq(
            self.vault_balance()?,
            expected_vault,
            &format!("vault balance unchanged after {suffix}"),
        )?;
        check_eq(
            self.total_committed()?,
            expected_committed,
            &format!("total_committed unchanged after {suffix}"),
        )?;
        Ok(())
    }

    fn check_ledger_rent(&self, wallet: &Pubkey, desc: &str) -> Result<()> {
        let pda = settlement_client::ledger_address(&self.program_id, wallet);
        let rent = self.rpc.get_minimum_balance_for_rent_exemption(40)?;
        check_eq(self.rpc.get_account(&pda)?.lamports, rent, desc)
    }

    fn check_wallet_left(
        &self,
        wallet: &Pubkey,
        cluster_before: u64,
        paid: u64,
        label: &str,
    ) -> Result<()> {
        let rent = self.rpc.get_minimum_balance_for_rent_exemption(40)?;
        let cluster_after = self.rpc.get_balance(wallet)?;
        check_eq(
            cluster_after,
            cluster_before + paid + rent,
            &format!("cluster balance {label} after leave"),
        )?;
        let pda = settlement_client::ledger_address(&self.program_id, wallet);
        ensure!(
            self.rpc.get_account(&pda).is_err(),
            "ledger {label} must be gone"
        );
        check_eq(
            self.rollup_balance(wallet)?,
            None,
            &format!("rollup balance {label} must be 404"),
        )?;
        Ok(())
    }
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

    let make_key = |name: &str| -> Result<(Keypair, PathBuf, String)> {
        let (kp, path) = (Keypair::new(), guard.dir.join(format!("{name}.json")));
        write_keypair_file(&kp, &path).map_err(|e| anyhow!("{e}"))?;
        let s = pstr(&path)?.to_string();
        Ok((kp, path, s))
    };
    let (sequencer_key, seq_file, seq_path) = make_key("sequencer")?;
    let (wallet_a_key, _a_file, a_path) = make_key("wallet-a")?;
    let (wallet_b_key, _b_file, b_path) = make_key("wallet-b")?;
    let (fresh_key, _fresh_file, fresh_path) = make_key("fresh")?;
    let (program_key, prog_file, _) = make_key("program")?;
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

    let binaries = target_directory().join("debug");
    let client_bin = binaries.join("rollup_client");
    let http = Client::builder().timeout(Duration::from_secs(5)).build()?;
    let ctx = SettleContext::new(rpc, http, program_id, client_bin);

    for pk in [&seq_pubkey, &a_pubkey, &b_pubkey] {
        ctx.airdrop(pk, 5_000_000_000)?;
    }

    let prog_str = program_id.to_string();
    for (cmd, path, amt) in [
        ("initialize", &seq_path, ""),
        ("deposit", &a_path, "2000000000"),
        ("deposit", &b_path, "1000000000"),
    ] {
        let mut args = vec![cmd, path.as_str(), &prog_str];
        if !amt.is_empty() {
            args.push(amt);
        }
        args.push("http://127.0.0.1:8899");
        ctx.client(&args)?;
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

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut rollup_ready = false;
    while Instant::now() < deadline {
        let roll = guard.rollup.as_mut().context("missing rollup")?;
        if let Some(status) = roll.try_wait()? {
            bail!("rollup_core exited early with {status}");
        }
        if let Ok(res) = ctx.http.get("http://127.0.0.1:8080/").send() {
            if res.status().is_success() && res.json::<Value>()?["settlement"] == "configured" {
                rollup_ready = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    ensure!(rollup_ready, "rollup_core failed to become ready in 30s");
    println!("ok: rollup started in settlement mode");

    check_eq(
        ctx.rollup_balance(&a_pubkey)?,
        Some(2_000_000_000),
        "initial rollup balance A",
    )?;
    check_eq(
        ctx.rollup_balance(&b_pubkey)?,
        Some(1_000_000_000),
        "initial rollup balance B",
    )?;

    let rent_min_0 = ctx.rpc.get_minimum_balance_for_rent_exemption(0)?;
    check_eq(
        ctx.vault_balance()?,
        rent_min_0 + 3_000_000_000,
        "initial cluster vault balance",
    )?;
    check_eq(
        ctx.total_committed()?,
        3_000_000_000,
        "initial total_committed",
    )?;
    println!("ok: initial balances verified");

    ctx.client(&[
        &a_path,
        &b_pubkey.to_string(),
        "500000000",
        "http://127.0.0.1:8080",
    ])?;
    check_eq(
        ctx.rollup_balance(&a_pubkey)?,
        Some(1_499_995_000),
        "balance A after transfer",
    )?;
    check_eq(
        ctx.rollup_balance(&b_pubkey)?,
        Some(1_500_000_000),
        "balance B after transfer",
    )?;
    println!("ok: transfer A to B verified");

    ctx.client_refusal(
        &[
            &a_path,
            &fresh_pubkey.to_string(),
            "100000000",
            "http://127.0.0.1:8080",
        ],
        "has not committed lamports to settlement",
        "fresh recipient",
    )?;
    check_eq(
        ctx.rollup_balance(&a_pubkey)?,
        Some(1_499_995_000),
        "A balance unchanged after unfunded transfer refusal",
    )?;
    ctx.client_refusal(
        &[
            &fresh_path,
            &b_pubkey.to_string(),
            "100000000",
            "http://127.0.0.1:8080",
        ],
        "has not committed lamports to settlement",
        "uncommitted sender",
    )?;

    let blockhash = ctx.blockhash()?;
    let bad_sig = sign_leave(&wallet_a_key, &b_pubkey, &blockhash);
    ctx.check_leave_status(
        &b_pubkey,
        &blockhash,
        &bad_sig,
        400,
        "leave with mismatched signature must be 400",
    )?;

    let fake_bh = solana_hash::Hash::new_unique().to_string();
    let fake_sig = sign_leave(&wallet_b_key, &b_pubkey, &fake_bh);
    ctx.check_leave_status(
        &b_pubkey,
        &fake_bh,
        &fake_sig,
        400,
        "leave with unknown blockhash must be 400",
    )?;
    println!("ok: refusals verified");

    let seq_cluster_before = ctx.rpc.get_balance(&seq_pubkey)?;
    let vault_bal_before = ctx.vault_balance()?;
    let committed_before = ctx.total_committed()?;

    let latest_bh = ctx.rpc.get_latest_blockhash()?;
    let fake_leave_ix = settlement_client::leave_instruction(&program_id, &a_pubkey, &a_pubkey, 0);
    let fake_leave_tx = Transaction::new_signed_with_payer(
        &[fake_leave_ix],
        Some(&a_pubkey),
        &[&wallet_a_key],
        latest_bh,
    );
    let err = ctx
        .rpc
        .send_and_confirm_transaction(&fake_leave_tx)
        .err()
        .context("leave not from sequencer must fail")?;
    let err_str = err.to_string();
    ensure!(
        err_str.contains("custom program error: 0x6"),
        "unexpected error for NotSequencer: {err_str}"
    );
    ctx.check_vault_and_committed(vault_bal_before, committed_before, "NotSequencer")?;
    println!("ok: leave not from the sequencer refused (NotSequencer)");

    ctx.client_refusal(
        &[
            "deposit",
            &a_path,
            &prog_str,
            "1000",
            "http://127.0.0.1:8899",
        ],
        "0x5",
        "second deposit",
    )?;
    ctx.check_vault_and_committed(vault_bal_before, committed_before, "AlreadyDeposited")?;
    println!("ok: second deposit refused (AlreadyDeposited)");

    ctx.check_ledger_rent(&b_pubkey, "ledger B balance before leave")?;
    let b_cluster_before = ctx.rpc.get_balance(&b_pubkey)?;
    let vault_before_b = ctx.vault_balance()?;

    let bh_b = ctx.blockhash()?;
    let sig_b = sign_leave(&wallet_b_key, &b_pubkey, &bh_b);
    let res = ctx.post_leave(&b_pubkey, &bh_b, &sig_b)?;
    check_eq(res.status().as_u16(), 200, "leave B status")?;
    let receipt_b: Value = res.json()?;
    check_eq(
        receipt_b["lamports"].as_u64(),
        Some(1_500_000_000),
        "leave B lamports",
    )?;

    ctx.check_wallet_left(&b_pubkey, b_cluster_before, 1_500_000_000, "B")?;
    check_eq(
        ctx.vault_balance()?,
        vault_before_b - 1_500_000_000,
        "vault balance after B leave",
    )?;

    let bh2 = ctx.blockhash()?;
    let sig2 = sign_leave(&wallet_b_key, &b_pubkey, &bh2);
    ctx.check_leave_status(&b_pubkey, &bh2, &sig2, 404, "second leave B must be 404")?;
    ctx.check_leave_status(
        &b_pubkey,
        &bh_b,
        &sig_b,
        409,
        "replaying leave B must be 409",
    )?;
    println!("ok: wallet B leave verified");

    ctx.check_ledger_rent(&a_pubkey, "ledger A balance before leave")?;
    let a_cluster_before = ctx.rpc.get_balance(&a_pubkey)?;

    let out = ctx.client(&["leave", &a_path, "http://127.0.0.1:8080"])?;
    let receipt_a: Value = serde_json::from_str(&out)?;
    check_eq(
        receipt_a["lamports"].as_u64(),
        Some(1_499_995_000),
        "leave A lamports",
    )?;

    ctx.check_wallet_left(&a_pubkey, a_cluster_before, 1_499_995_000, "A")?;

    check_eq(
        ctx.vault_balance()?,
        rent_min_0 + 5_000,
        "vault balance after A leave",
    )?;
    check_eq(
        ctx.total_committed()?,
        5_000,
        "total_committed after A leave",
    )?;
    check_eq(
        ctx.rpc.get_balance(&seq_pubkey)?,
        seq_cluster_before - 10_000,
        "sequencer cluster balance dropped by 2 cluster fees",
    )?;
    println!("ok: wallet A leave verified");

    Ok(())
}
