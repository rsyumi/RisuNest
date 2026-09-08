use risunest_sync_server::{config::Config, http, store::Store};
use std::{path::PathBuf, sync::Arc};

const USAGE: &str = "risunest-sync-server <init|status|serve|device add|device revoke ID> --data-dir ABSOLUTE_PATH [--listen 127.0.0.1:4319] [--https-proxy]\nAdministration commands require the daemon to be stopped. Serve is loopback-only; use a trusted HTTPS reverse proxy for remote clients.";

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_none_or(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let command = args.next().unwrap();
    let subcommand = if command == "device" {
        Some(args.next().ok_or(USAGE)?)
    } else {
        None
    };
    let revoke = if subcommand.as_deref() == Some("revoke") {
        Some(args.next().ok_or(USAGE)?)
    } else {
        None
    };
    let mut data_dir = None;
    let mut listen = "127.0.0.1:4319".parse()?;
    let mut https_proxy = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" if data_dir.is_none() => {
                data_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?))
            }
            "--listen" => listen = args.next().ok_or(USAGE)?.parse()?,
            "--https-proxy" => https_proxy = true,
            _ => return Err(USAGE.into()),
        }
    }
    let config = Config {
        data_dir: data_dir.ok_or(USAGE)?,
        listen,
        https_proxy,
    };
    config.validate()?;
    if !["init", "status", "serve", "device"].contains(&command.as_str()) {
        return Err(USAGE.into());
    }
    let store = if command == "init" {
        Store::init(&config.data_dir)?
    } else {
        Store::open(&config.data_dir)?
    };
    match command.as_str() {
        "init" | "status" => println!("{}", serde_json::to_string(&store.head()?)?),
        "device" => match subcommand.as_deref() {
            Some("add") => println!("{}", serde_json::to_string(&store.add_device()?)?),
            Some("revoke") => store.revoke_device(&revoke.unwrap())?,
            _ => return Err(USAGE.into()),
        },
        "serve" => {
            let listener = tokio::net::TcpListener::bind(config.listen).await?;
            eprintln!(
                "sync listener ready: {} ({})",
                listener.local_addr()?,
                if config.https_proxy {
                    "HTTPS proxy origin"
                } else {
                    "local development"
                }
            );
            axum::serve(listener, http::router(Arc::new(store)))
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        _ => unreachable!(),
    }
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! { _=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
