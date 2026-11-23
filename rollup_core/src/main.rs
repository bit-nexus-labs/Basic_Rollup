use std::{env, path::Path, str::FromStr, thread};

use actix_web::{web, App, HttpServer};
use anyhow::{anyhow, ensure, Context, Result};
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;

use crate::{frontend::Frontend, rollupdb::RollupDB, sequencer::Sequencer, settle::Settlement};

mod frontend;
mod rollupdb;
mod sequencer;
mod settle;

#[actix_web::main]
async fn main() -> Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    let genesis = env::var("ROLLUP_GENESIS").ok();
    let cluster_url = env::var("ROLLUP_CLUSTER_URL").ok();
    let settlement_program = env::var("ROLLUP_SETTLEMENT_PROGRAM").ok();
    let sequencer_keypair = env::var("ROLLUP_SEQUENCER_KEYPAIR").ok();

    ensure!(
        settlement_program.is_some() == sequencer_keypair.is_some(),
        "both ROLLUP_SETTLEMENT_PROGRAM and ROLLUP_SEQUENCER_KEYPAIR must be set, or neither"
    );

    let settlement = if let (Some(prog_str), Some(key_path)) =
        (&settlement_program, &sequencer_keypair)
    {
        ensure!(
            cluster_url.is_some(),
            "ROLLUP_CLUSTER_URL is required when settlement is configured"
        );
        ensure!(
            genesis.is_none(),
            "ROLLUP_GENESIS cannot be used when settlement is configured"
        );
        let program = Pubkey::from_str(prog_str).context("invalid ROLLUP_SETTLEMENT_PROGRAM")?;
        let cluster = cluster_url.as_deref().unwrap().to_string();
        let path_buf = Path::new(key_path).to_path_buf();
        let handle = thread::spawn(move || Settlement::connect(&cluster, program, &path_buf));
        Some(
            handle
                .join()
                .map_err(|_| anyhow!("settlement connect panicked"))??,
        )
    } else {
        ensure!(
            genesis.is_some() || cluster_url.is_some(),
            "set ROLLUP_GENESIS to a genesis JSON file, ROLLUP_CLUSTER_URL to a Solana RPC URL \
             to pull accounts from, or both"
        );
        None
    };
    let settlement_configured = settlement.is_some();
    let bind = env::var("ROLLUP_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let mut db = RollupDB::new(cluster_url.as_deref());
    if let Some(genesis) = &genesis {
        db.load_genesis(Path::new(genesis))?;
    }
    let sequencer = Sequencer::new(db, settlement)?;
    let (sender, receiver) = mpsc::channel(128);
    let frontend = web::Data::new(Frontend {
        sequencer: sender,
        settlement_configured,
    });
    let server = HttpServer::new(move || {
        App::new()
            .app_data(frontend.clone())
            .app_data(web::JsonConfig::default().limit(32 * 1024))
            .route("/", web::get().to(frontend::health))
            .route(
                "/get_latest_blockhash",
                web::get().to(frontend::get_latest_blockhash),
            )
            .route(
                "/get_account/{address}",
                web::get().to(frontend::get_account),
            )
            .route(
                "/get_transaction",
                web::post().to(frontend::get_transaction),
            )
            .route(
                "/submit_transaction",
                web::post().to(frontend::submit_transaction),
            )
            .route("/leave", web::post().to(frontend::leave))
    })
    .bind(&bind)
    .with_context(|| format!("binding HTTP server to {bind}"))?;
    // Agave: `BankingStage::spawn_internal_central`, starts the scheduler thread and
    // its worker threads. The rollup's one thread is both.
    // https://github.com/anza-xyz/agave/blob/v4.4.1/core/src/banking_stage.rs#L509
    let worker = thread::Builder::new()
        .name("sequencer".into())
        .spawn(move || sequencer.run(receiver))?;
    if let Some(program_id) = &settlement_program {
        log::info!(
            "SOL-transfer demo listening at http://{bind}; in-memory state, \
             L1 settlement configured, program {program_id}"
        );
    } else {
        log::info!(
            "SOL-transfer demo listening at http://{bind}; in-memory state, accounts pulled from {}, \
             L1 settlement not configured",
            cluster_url.as_deref().unwrap_or("genesis only")
        );
    }
    let result = server.run().await;
    worker
        .join()
        .map_err(|_| anyhow!("sequencer thread panicked"))?;
    result.context("HTTP server stopped with an error")
}
