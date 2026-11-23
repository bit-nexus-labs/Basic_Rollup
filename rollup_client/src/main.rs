use std::{env, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use reqwest::{Client, Response};
use serde::Serialize;
use serde_json::{json, Value};
use settlement_client::{deposit_instruction, initialize_instruction};
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair as keypair;
use solana_message::{
    v1::{Message, TransactionConfig},
    VersionedMessage,
};
use solana_pubkey::Pubkey;
use solana_rpc_client::{api::config::CommitmentConfig, rpc_client::RpcClient};
use solana_signature::Signature;
use solana_signer::Signer;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::{versioned::VersionedTransaction, Transaction};

/// A v1 transaction states its own limits; anything left unset is zero, not a default.
/// The System program charges 150 compute units for a transfer, and one 32 KiB page
/// covers the three accounts it loads (64 bytes each plus their data).
const TRANSFER_CONFIG: TransactionConfig = TransactionConfig::empty()
    .with_compute_unit_limit(150)
    .with_loaded_accounts_data_size_limit(32 * 1024);

const USAGE: &str = "\
Usage: rollup_client <keypair.json> <recipient-pubkey> <lamports> [rollup-url]
       rollup_client initialize <sequencer-keypair.json> <program-id> [cluster-url]
       rollup_client deposit <wallet-keypair.json> <program-id> <lamports> [cluster-url]";

#[derive(Serialize)]
struct RollupTransaction {
    sender: String,
    /// Base64 of the serialized transaction, the form Solana's `sendTransaction` takes.
    sol_transaction: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = env::args().skip(1).collect();

    match arguments.first().map(String::as_str) {
        Some("initialize") => {
            if !(3..=4).contains(&arguments.len()) {
                bail!("{USAGE}");
            }
            let payer = keypair::read_keypair_file(&arguments[1])
                .map_err(|error| anyhow!("failed to read keypair {}: {error}", arguments[1]))?;
            let program_id: Pubkey = arguments[2].parse().context("invalid program id")?;
            let cluster_url = arguments
                .get(3)
                .map(String::as_str)
                .unwrap_or("http://127.0.0.1:8899")
                .to_string();
            let instruction = initialize_instruction(&program_id, &payer.pubkey(), &payer.pubkey());
            send_settlement_transaction(payer, instruction, cluster_url).await
        }
        Some("deposit") => {
            if !(4..=5).contains(&arguments.len()) {
                bail!("{USAGE}");
            }
            let payer = keypair::read_keypair_file(&arguments[1])
                .map_err(|error| anyhow!("failed to read keypair {}: {error}", arguments[1]))?;
            let program_id: Pubkey = arguments[2].parse().context("invalid program id")?;
            let lamports: u64 = arguments[3]
                .parse()
                .context("lamports must be a positive integer")?;
            let cluster_url = arguments
                .get(4)
                .map(String::as_str)
                .unwrap_or("http://127.0.0.1:8899")
                .to_string();
            let instruction = deposit_instruction(&program_id, &payer.pubkey(), lamports);
            send_settlement_transaction(payer, instruction, cluster_url).await
        }
        _ => {
            if !(3..=4).contains(&arguments.len()) {
                bail!("{USAGE}");
            }
            run_transfer(&arguments).await
        }
    }
}

async fn send_settlement_transaction(
    payer: keypair::Keypair,
    instruction: Instruction,
    cluster_url: String,
) -> Result<()> {
    let signature = tokio::task::spawn_blocking(move || -> Result<Signature> {
        let client = RpcClient::new_with_commitment(cluster_url, CommitmentConfig::confirmed());
        let blockhash = client.get_latest_blockhash().map_err(|e| anyhow!("{e}"))?;
        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );
        client
            .send_and_confirm_transaction(&transaction)
            .map_err(|e| anyhow!("{e}"))
    })
    .await
    .context("join blocking settlement task")??;

    println!("{signature}");
    Ok(())
}

async fn run_transfer(arguments: &[String]) -> Result<()> {
    let recipient: Pubkey = arguments[1]
        .parse()
        .context("invalid recipient public key")?;
    let lamports: u64 = arguments[2]
        .parse()
        .context("lamports must be a positive integer")?;
    if lamports == 0 {
        bail!("lamports must be greater than zero");
    }
    let payer = keypair::read_keypair_file(&arguments[0])
        .map_err(|error| anyhow!("failed to read keypair {}: {error}", arguments[0]))?;
    let rollup_url = arguments
        .get(3)
        .map(String::as_str)
        .unwrap_or("http://127.0.0.1:8080")
        .trim_end_matches('/');
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("create HTTP client")?;

    let response = client
        .get(format!("{rollup_url}/get_latest_blockhash"))
        .send()
        .await
        .context("request rollup blockhash")?;
    let latest_blockhash = read_response(response).await?;
    let blockhash: Hash = latest_blockhash["blockhash"]
        .as_str()
        .context("rollup response is missing blockhash")?
        .parse()
        .context("rollup returned an invalid blockhash")?;

    // Build and sign a v1 transaction, the current Mainnet format (up to 4096 bytes).
    let instruction = system_instruction::transfer(&payer.pubkey(), &recipient, lamports);
    let message = Message::try_compile_with_config(
        &payer.pubkey(),
        &[instruction],
        blockhash,
        TRANSFER_CONFIG,
    )
    .context("compile transfer message")?;
    let transaction = VersionedTransaction::try_new(VersionedMessage::V1(message), &[&payer])
        .context("sign transfer")?;
    let wire_transaction = wincode::serialize(&transaction).context("serialize transfer")?;

    let request = RollupTransaction {
        sender: payer.pubkey().to_string(),
        sol_transaction: BASE64_STANDARD.encode(wire_transaction),
    };
    let response = client
        .post(format!("{rollup_url}/submit_transaction"))
        .json(&request)
        .send()
        .await
        .context("submit transfer to rollup")?;
    let receipt = read_response(response).await?;
    if receipt["status"].as_str() != Some("executed") {
        bail!("rollup did not execute the transfer: {receipt}");
    }

    let signature = receipt["signature"]
        .as_str()
        .context("rollup receipt is missing transaction signature")?;
    let response = client
        .post(format!("{rollup_url}/get_transaction"))
        .json(&json!({ "get_tx": signature }))
        .send()
        .await
        .context("retrieve transfer receipt")?;
    let transaction_record = read_response(response).await?;
    println!("{}", serde_json::to_string_pretty(&transaction_record)?);
    Ok(())
}

async fn read_response(response: Response) -> Result<Value> {
    let status = response.status();
    let body = response.text().await.context("read rollup response")?;
    if !status.is_success() {
        bail!("rollup returned HTTP {status}: {body}");
    }
    serde_json::from_str(&body).context("rollup returned invalid JSON")
}
