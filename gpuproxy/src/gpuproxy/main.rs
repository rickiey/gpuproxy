use crate::db_ops::*;
use crate::worker::Worker;
use clap::{Arg, Command};
use gpuproxy::cli;
use gpuproxy::config::*;
use gpuproxy::proxy_rpc::rpc::ProxyImpl;
use gpuproxy::proxy_rpc::*;
use gpuproxy::resource;
use jsonrpsee::http_server::{HttpServerBuilder, HttpServerHandle, RpcModule};
use log::*;
use sea_orm::{ConnectOptions, Database};
use simplelog::*;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use migration::{Migrator, MigratorTrait};

#[tokio::main()]
async fn main() {
    let worker_args = cli::worker::get_worker_arg();
    let list_task_cmds = cli::tasks::list_task_cmds().await;
    let fetch_params_cmds = cli::params_fetch_cli::fetch_params_cmds().await;
    let app_m = Command::new("gpuproxy")
        .version("0.0.1")
        .args(&[
            Arg::new("url")
                .long("url")
                .env("C2PROXY_URL")
                .global(true)
                .default_value("127.0.0.1:18888")
                .required(false)
                .help("specify url for provide service api service"),
            Arg::new("log-level")
                .long("log-level")
                .global(true)
                .env("C2PROXY_LOG_LEVEL")
                .default_value("info")
                .help("set log level for application"),
        ])
        .arg_required_else_help(true)
        .subcommand(
            Command::new("run")
                .about("run daemon for provide service")
                .args(&[
                    Arg::new("db-dsn")
                        .long("db-dsn")
                        .env("C2PROXY_DSN")
                        .default_value("sqlite://gpuproxy.db")
                        .help("specify sqlite path to store task"),
                    Arg::new("max-c2")
                        .long("max-c2")
                        .env("C2PROXY_MAX_C2")
                        .default_value("1")
                        .help("number of c2 task to run parallelly"),
                    Arg::new("disable-worker")
                        .long("disable-worker")
                        .env("C2PROXY_DISABLE_WORKER")
                        .required(false)
                        .takes_value(false)
                        .default_value("false")
                        .help("disable worker on gpuproxy manager"),
                    Arg::new("resource-type")
                        .long("resource-type")
                        .env("C2PROXY_RESOURCE_TYPE")
                        .default_value("db")
                        .help("resource type(db, fs)"),
                    Arg::new("fs-resource-path")
                        .long("fs-resource-path")
                        .env("C2PROXY_FS_RESOURCE_PATH")
                        .default_value("")
                        .help("when resource type is fs, will use this path to read resource"),
                ])
                .args(worker_args),
        )
        .subcommand(list_task_cmds)
        .subcommand(fetch_params_cmds)
        .get_matches();

    match app_m.subcommand() {
        Some(("run", ref sub_m)) => {
            cli::worker::set_worker_env(sub_m);

            let url: String = sub_m.value_of_t("url").unwrap_or_else(|e| e.exit());
            let max_c2: usize = sub_m.value_of_t("max-c2").unwrap_or_else(|e| e.exit());
            let db_dsn: String = sub_m.value_of_t("db-dsn").unwrap_or_else(|e| e.exit());
            let log_level: String = sub_m.value_of_t("log-level").unwrap_or_else(|e| e.exit());
            let resource_type: String = sub_m.value_of_t("resource-type").unwrap_or_else(|e| e.exit());
            let fs_resource_type: String = sub_m.value_of_t("fs-resource-path").unwrap_or_else(|e| e.exit());
            let disable_worker: bool = sub_m.value_of_t("disable-worker").unwrap_or_else(|e| e.exit());
            let cfg = ServiceConfig::new(url, db_dsn, max_c2, disable_worker, resource_type, fs_resource_type, log_level.clone());

            let lv = LevelFilter::from_str(cfg.log_level.as_str()).unwrap();
            TermLogger::init(lv, Config::default(), TerminalMode::Mixed, ColorChoice::Auto).unwrap();

            run_cfg(cfg).await;
        } // run was used
        Some(("tasks", ref sub_m)) => cli::tasks::tasks_command(sub_m).await, // task was used
        Some(("paramfetch", ref sub_m)) => cli::params_fetch_cli::fetch_params_command(sub_m).await, // run was used
        _ => {}                                                               // Either no subcommand or one not tested for...
    }
}

async fn run_cfg(cfg: ServiceConfig) {
    let mut opt = ConnectOptions::new(cfg.db_dsn);
    opt.max_connections(10)
        .min_connections(5)
        .sqlx_logging(false)
        .max_lifetime(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(8))
        .idle_timeout(Duration::from_secs(8));

    let db_conn = Database::connect(opt).await.unwrap();
    Migrator::up(&db_conn, None).await.unwrap();

    let db_ops = db_ops::DbOpsImpl::new(db_conn);
    let worker_id = db_ops.get_worker_id().await.unwrap();
    let arc_pool = Arc::new(db_ops);

    let resource: Arc<dyn resource::Resource + Send + Sync> = match cfg.resource {
        Resource::Db => arc_pool.clone(),
        Resource::FS(path) => Arc::new(resource::FileResource::new(path)),
    };

    let worker = worker::LocalWorker::new(cfg.max_c2, worker_id.to_string(), resource.clone(), arc_pool.clone());

    let rpc_module = rpc::register(resource, arc_pool);
    if !cfg.disable_worker {
        worker.process_tasks().await;
        info!("ready for local worker address worker_id {}", worker_id);
    }

    println!("{}", cfg.url.clone());
    let (server_addr, _handle) = run_server(cfg.url.as_str(), rpc_module).await.unwrap();
    info!("starting listening {}", server_addr);
    let () = futures::future::pending().await;
    info!("Shutting Down");
} //run cfg

async fn run_server(url:&str, module: RpcModule<ProxyImpl>) -> anyhow::Result<(SocketAddr, HttpServerHandle)> {
    let server = HttpServerBuilder::default().build(url.parse::<SocketAddr>()?)?;

    let addr = server.local_addr()?;
    let server_handle = server.start(module)?;

    Ok((addr, server_handle))
}
