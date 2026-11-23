use std::path::Path;

use anyhow::{anyhow, ensure, Context, Result};
use serde::Serialize;
use solana_keypair::{read_keypair_file, Keypair};
use solana_pubkey::Pubkey;
use solana_rpc_client::{api::config::CommitmentConfig, rpc_client::RpcClient};
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;

/// Local execution is not an L1 settlement proof.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettlementStatus {
    NotConfigured,
    /// The lamports are committed on the cluster; they are paid out when the wallet leaves.
    Pending,
}

pub(crate) struct Settlement {
    program: Pubkey,
    sequencer: Keypair,
    cluster: RpcClient,
}

impl Settlement {
    pub fn connect(cluster_url: &str, program: Pubkey, keypair_path: &Path) -> Result<Self> {
        let sequencer = read_keypair_file(keypair_path).map_err(|e| {
            anyhow!(
                "failed to read sequencer keypair {}: {e}",
                keypair_path.display()
            )
        })?;
        let cluster = RpcClient::new_with_commitment(cluster_url, CommitmentConfig::confirmed());

        let config_addr = settlement_client::config_address(&program);
        let config_account = cluster
            .get_account(&config_addr)
            .context("run rollup_client initialize first")?;
        ensure!(
            config_account.owner == program,
            "run rollup_client initialize first"
        );
        let (stored_sequencer, _) = settlement_client::parse_config(&config_account.data)
            .context("run rollup_client initialize first")?;
        ensure!(
            stored_sequencer == sequencer.pubkey(),
            "run rollup_client initialize first"
        );

        Ok(Self {
            program,
            sequencer,
            cluster,
        })
    }

    pub fn deposits_of(&self, wallets: &[Pubkey]) -> Result<Vec<Option<u64>>> {
        if wallets.is_empty() {
            return Ok(Vec::new());
        }
        let ledger_addrs: Vec<Pubkey> = wallets
            .iter()
            .map(|w| settlement_client::ledger_address(&self.program, w))
            .collect();
        let accounts = self
            .cluster
            .get_multiple_accounts(&ledger_addrs)
            .context("fetching ledgers from cluster")?;
        let mut results = Vec::with_capacity(wallets.len());
        for (wallet, account) in wallets.iter().zip(accounts) {
            let deposit = account.and_then(|acc| {
                if acc.owner == self.program {
                    settlement_client::parse_ledger(&acc.data).and_then(|(stored_w, dep)| {
                        if stored_w == *wallet {
                            Some(dep)
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            });
            results.push(deposit);
        }
        Ok(results)
    }

    pub fn deposit_of(&self, wallet: &Pubkey) -> Result<Option<u64>> {
        let mut deposits = self.deposits_of(std::slice::from_ref(wallet))?;
        Ok(deposits.pop().unwrap())
    }

    pub fn leave(&self, wallet: &Pubkey, amount: u64) -> Result<Signature> {
        let instruction = settlement_client::leave_instruction(
            &self.program,
            &self.sequencer.pubkey(),
            wallet,
            amount,
        );
        let blockhash = self
            .cluster
            .get_latest_blockhash()
            .context("fetching blockhash for leave")?;
        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&self.sequencer.pubkey()),
            &[&self.sequencer],
            blockhash,
        );
        self.cluster
            .send_and_confirm_transaction(&transaction)
            .map_err(|e| anyhow!("{e}"))
    }
}
