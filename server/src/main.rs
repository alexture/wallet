use anyhow::{Context, Result};
use app::{AppModule, AppModuleCtx, AppOutWsEvent, AppWsInMessage};
use axum::Router;
use clap::Parser;
use client_sdk::{
    helpers::risc0::Risc0Prover,
    rest_client::{IndexerApiHttpClient, NodeApiHttpClient},
};
use conf::Conf;
use history::{HistoryEvent, HyllarHistory};
use hyle_modules::{
    bus::{metrics::BusMetrics, SharedMessageBus},
    modules::{
        contract_state_indexer::{ContractStateIndexer, ContractStateIndexerCtx},
        da_listener::{DAListener, DAListenerConf},
        prover::{AutoProver, AutoProverCtx},
        rest::{RestApi, RestApiRunContext},
        websocket::WebSocketModule,
        BuildApiContextInner, ModulesHandler,
    },
    utils::logger::setup_tracing,
};

use hyle_smt_token::client::tx_executor_handler::SmtTokenProvableState;
use prometheus::Registry;
use sdk::{api::NodeInfo, info, ContractName, ZkContract};
use std::sync::{Arc, Mutex};
use tracing::error;
use wallet::{client::indexer::WalletEvent, Wallet};

mod app;
mod conf;
mod history;
mod init;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[arg(long, default_value = "config.toml")]
    pub config_file: Vec<String>,

    #[arg(long, default_value = "wallet")]
    pub wallet_cn: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Conf::new(args.config_file).context("reading config file")?;

    setup_tracing(
        &config.log_format,
        format!("{}(nopkey)", config.id.clone(),),
    )
    .context("setting up tracing")?;

    let config = Arc::new(config);

    info!("Starting app with config: {:?}", &config);

    let node_client =
        Arc::new(NodeApiHttpClient::new(config.node_url.clone()).context("build node client")?);
    let indexer_client = Arc::new(
        IndexerApiHttpClient::new(config.indexer_url.clone()).context("build indexer client")?,
    );

    let wallet_cn: ContractName = args.wallet_cn.clone().into();

    let contracts = vec![init::ContractInit {
        name: wallet_cn.clone(),
        program_id: contracts::WALLET_ID,
        initial_state: Wallet::default().commit(),
    }];

    match init::init_node(node_client.clone(), indexer_client.clone(), contracts).await {
        Ok(_) => {}
        Err(e) => {
            error!("Error initializing node: {:?}", e);
            return Ok(());
        }
    }
    let bus = SharedMessageBus::new(BusMetrics::global(config.id.clone()));

    std::fs::create_dir_all(&config.data_directory).context("creating data directory")?;

    let mut handler = ModulesHandler::new(&bus).await;

    let api_ctx = Arc::new(BuildApiContextInner {
        router: Mutex::new(Some(Router::new())),
        openapi: Default::default(),
    });

    let app_ctx = Arc::new(AppModuleCtx {
        api: api_ctx.clone(),
        node_client,
        wallet_cn: wallet_cn.clone(),
    });

    handler.build_module::<AppModule>(app_ctx.clone()).await?;

    handler
        .build_module::<ContractStateIndexer<Wallet, WalletEvent>>(ContractStateIndexerCtx {
            contract_name: wallet_cn.clone(),
            data_directory: config.data_directory.clone(),
            api: api_ctx.clone(),
        })
        .await?;
    handler
        .build_module::<ContractStateIndexer<HyllarHistory, Vec<HistoryEvent>>>(
            ContractStateIndexerCtx {
                contract_name: "oranj".into(),
                data_directory: config.data_directory.clone(),
                api: api_ctx.clone(),
            },
        )
        .await?;

    handler
        .build_module::<AutoProver<Wallet>>(Arc::new(AutoProverCtx {
            data_directory: config.data_directory.clone(),
            prover: Arc::new(Risc0Prover::new(contracts::WALLET_ELF)),
            contract_name: wallet_cn.clone(),
            node: app_ctx.node_client.clone(),
            default_state: Default::default(),
            buffer_blocks: config.wallet_buffer_blocks,
            max_txs_per_proof: config.wallet_max_txs_per_proof,
        }))
        .await?;
    handler
        .build_module::<AutoProver<SmtTokenProvableState>>(Arc::new(AutoProverCtx {
            data_directory: config.data_directory.clone(),
            prover: Arc::new(Risc0Prover::new(
                hyle_smt_token::client::tx_executor_handler::metadata::SMT_TOKEN_ELF,
            )),
            contract_name: "oranj".into(),
            node: app_ctx.node_client.clone(),
            default_state: Default::default(),
            buffer_blocks: config.smt_buffer_blocks,
            max_txs_per_proof: config.smt_max_txs_per_proof,
        }))
        .await?;

    handler
        .build_module::<WebSocketModule<AppWsInMessage, AppOutWsEvent>>(config.websocket.clone())
        .await?;

    // This module connects to the da_address and receives all the blocks²
    handler
        .build_module::<DAListener>(DAListenerConf {
            start_block: None,
            data_directory: config.data_directory.clone(),
            da_read_from: config.da_read_from.clone(),
        })
        .await?;

    // Should come last so the other modules have nested their own routes.
    #[allow(clippy::expect_used, reason = "Fail on misconfiguration")]
    let router = api_ctx
        .router
        .lock()
        .expect("Context router should be available.")
        .take()
        .expect("Context router should be available.");
    #[allow(clippy::expect_used, reason = "Fail on misconfiguration")]
    let openapi = api_ctx
        .openapi
        .lock()
        .expect("OpenAPI should be available")
        .clone();

    handler
        .build_module::<RestApi>(RestApiRunContext {
            port: config.rest_server_port,
            max_body_size: config.rest_server_max_body_size,
            registry: Registry::new(),
            router,
            openapi,
            info: NodeInfo {
                id: config.id.clone(),
                da_address: config.da_read_from.clone(),
                pubkey: None,
            },
        })
        .await?;

    #[cfg(unix)]
    {
        use tokio::signal::unix;
        let mut terminate = unix::signal(unix::SignalKind::interrupt())?;
        tokio::select! {
            Err(e) = handler.start_modules() => {
                error!("Error running modules: {:?}", e);
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down");
            }
            _ = terminate.recv() =>  {
                info!("SIGTERM received, shutting down");
            }
        }
        _ = handler.shutdown_modules().await;
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            Err(e) = handler.start_modules() => {
                error!("Error running modules: {:?}", e);
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down");
            }
        }
        _ = handler.shutdown_modules().await;
    }

    Ok(())
}
