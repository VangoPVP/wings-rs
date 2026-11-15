use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing_subscriber::EnvFilter;
use wings::{
    activity::ActivityLog,
    config::{Config, DEFAULT_CONFIG_PATH},
    context::{App, Ctx},
    environment,
    jwt::JwtValidator,
    remote::RemoteClient,
    router,
    server::manager::Manager,
};

struct Args {
    config: PathBuf,
    debug: bool,
}

fn parse_args() -> Result<Args> {
    let mut config = std::env::var("WINGS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| DEFAULT_CONFIG_PATH.into());
    let mut debug = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => config = it.next().context("--config needs a path")?.into(),
            "--debug" => debug = true,
            "--help" | "-h" => {
                println!(
                    "usage: wings [--config <path>] [--debug]\n       \
                     wings openapi [wings|remote]   print an OpenAPI spec as JSON"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(Args { config, debug })
}

fn print_openapi(which: Option<&str>) -> Result<()> {
    let spec = match which.unwrap_or("wings") {
        "wings" => wings::openapi::wings_spec(),
        "remote" => wings::openapi::remote_spec(),
        other => anyhow::bail!("unknown spec '{other}' (expected 'wings' or 'remote')"),
    };
    println!("{}", spec.to_pretty_json()?);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("openapi") {
        return print_openapi(argv.get(1).map(String::as_str));
    }
    let args = parse_args()?;
    let config = Config::load(&args.config)?;
    let debug = args.debug || config.debug;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(if debug { "wings=debug,info" } else { "info" })),
        )
        .init();

    config.ensure_directories()?;
    let config = Arc::new(config);

    let docker = bollard::Docker::connect_with_socket_defaults().context("connecting to docker")?;
    docker.ping().await.context("docker is not reachable")?;
    environment::ensure_network(&docker, &config).await?;

    let remote = RemoteClient::new(&config)?;
    let ctx = Arc::new(Ctx {
        config: config.clone(),
        docker,
        remote,
        activity: ActivityLog::default(),
        states_changed: tokio::sync::Notify::new(),
    });
    let app = Arc::new(App {
        ctx: ctx.clone(),
        manager: Manager::new(ctx.clone()),
        jwt: JwtValidator::new(&config.token)?,
        started_at: Instant::now(),
    });

    {
        let a = app.clone();
        tokio::spawn(async move { a.manager.boot_with_retry().await });
        let a = app.clone();
        tokio::spawn(async move { a.manager.run_state_persister().await });
        let a = app.clone();
        let interval = Duration::from_secs(config.system.state_report_interval);
        tokio::spawn(async move { a.manager.run_state_reporter(interval).await });
        let a = app.clone();
        tokio::spawn(async move { a.activity.run_flusher(&a.remote, Duration::from_secs(10)).await });
    }

    if config.system.sftp.enabled {
        let a = app.clone();
        tokio::spawn(async move {
            if let Err(e) = wings::sftp::run(a).await {
                tracing::error!(error = ?e, "sftp server stopped");
            }
        });
    }

    let addr: SocketAddr = format!("{}:{}", config.api.host, config.api.port)
        .parse()
        .context("invalid api.host/api.port")?;
    let router = router::build(app.clone());
    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        let app = app.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            app.manager.save_states();
            app.activity.flush(&app.remote).await;
            handle.graceful_shutdown(Some(Duration::from_secs(10)));
        });
    }

    let svc = router.into_make_service_with_connect_info::<SocketAddr>();
    if config.api.ssl.enabled {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            &config.api.ssl.cert,
            &config.api.ssl.key,
        )
        .await
        .context("loading TLS certificate")?;
        tracing::info!(%addr, "API listening (https)");
        axum_server::bind_rustls(addr, tls).handle(handle).serve(svc).await?;
    } else {
        tracing::info!(%addr, "API listening (http)");
        axum_server::bind(addr).handle(handle).serve(svc).await?;
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
}
