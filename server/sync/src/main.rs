#![cfg_attr(
    all(windows, feature = "windows-background"),
    windows_subsystem = "windows"
)]

use risunest_sync_server::{
    config::Config, connection::ConnectionOptions, http, management::Management, store::Store,
};
use std::{path::PathBuf, sync::Arc};

mod update;

const USAGE: &str = "risunest-sync-server <init|status|serve|maintain|backup|restore|restore-epoch|device add [--qr]|device revoke ID|connection configure|connection status|connection repost> --data-dir ABSOLUTE_PATH [--backup-dir ABSOLUTE_PATH] [--listen 127.0.0.1:4319] [--https-proxy]\nrisunest-sync-server update check\nconnection configure: choose --endpoint HTTPS_URL or --cloudflared ABSOLUTE_EXECUTABLE, optionally --registry REGISTRY_URL.\nUpdate check verifies the signed product catalog and reports the raw package for this OS and architecture without downloading or installing it.\nAdministration commands require the daemon to be stopped. Configured device add emits a private registration URI; --qr also displays its QR. Without connection configuration, manual credential JSON remains available. Backup and restore require a new destination directory. Serve is loopback-only.";

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
    let subcommand = if command == "device" || command == "connection" || command == "update" {
        Some(args.next().ok_or(USAGE)?)
    } else {
        None
    };
    if command == "update" {
        if subcommand.as_deref() != Some("check") || args.next().is_some() {
            return Err(USAGE.into());
        }
        let status = update::check().await.map_err(|error| error.to_owned())?;
        println!("{}", serde_json::to_string(&status)?);
        return Ok(());
    }
    let revoke = if subcommand.as_deref() == Some("revoke") {
        Some(args.next().ok_or(USAGE)?)
    } else {
        None
    };
    let mut data_dir = None;
    let mut backup_dir = None;
    let mut listen = "127.0.0.1:4319".parse()?;
    let mut https_proxy = false;
    let mut endpoint = None;
    let mut cloudflared = None;
    let mut registry_url = None;
    let mut qr = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" if data_dir.is_none() => {
                data_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?))
            }
            "--listen" => listen = args.next().ok_or(USAGE)?.parse()?,
            "--backup-dir" if backup_dir.is_none() => {
                backup_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?))
            }
            "--https-proxy" => https_proxy = true,
            "--endpoint"
                if command == "connection"
                    && subcommand.as_deref() == Some("configure")
                    && endpoint.is_none() =>
            {
                endpoint = Some(args.next().ok_or(USAGE)?)
            }
            "--cloudflared"
                if command == "connection"
                    && subcommand.as_deref() == Some("configure")
                    && cloudflared.is_none() =>
            {
                cloudflared = Some(PathBuf::from(args.next().ok_or(USAGE)?))
            }
            "--registry"
                if command == "connection"
                    && subcommand.as_deref() == Some("configure")
                    && registry_url.is_none() =>
            {
                registry_url = Some(args.next().ok_or(USAGE)?)
            }
            "--qr" if command == "device" && subcommand.as_deref() == Some("add") && !qr => {
                qr = true
            }
            _ => return Err(USAGE.into()),
        }
    }
    let config = Config {
        data_dir: data_dir.ok_or(USAGE)?,
        listen,
        https_proxy,
    };
    config.validate()?;
    if ![
        "init",
        "status",
        "serve",
        "maintain",
        "restore-epoch",
        "backup",
        "restore",
        "device",
        "connection",
    ]
    .contains(&command.as_str())
    {
        return Err(USAGE.into());
    }
    let store = if command == "restore" {
        Store::restore_backup(backup_dir.as_deref().ok_or(USAGE)?, &config.data_dir)?
    } else if command == "init" {
        Store::init(&config.data_dir)?
    } else {
        Store::open(&config.data_dir)?
    };
    match command.as_str() {
        "init" | "status" | "restore" => println!("{}", serde_json::to_string(&store.head()?)?),
        "backup" => println!(
            "{}",
            serde_json::to_string(&store.backup(backup_dir.as_deref().ok_or(USAGE)?)?)?
        ),
        "maintain" => println!("{}", serde_json::to_string(&store.maintain()?)?),
        "restore-epoch" => {
            store.rotate_restored_epoch()?;
            println!("{}", serde_json::to_string(&store.head()?)?);
        }
        "device" => match subcommand.as_deref() {
            Some("add") => {
                let connection = store.connection_status()?;
                // Offline issuance outlives this process; a Quick Tunnel changes on restart.
                if connection.mode == "managed" && !connection.directory_enabled {
                    return Err(risunest_sync_server::Error::new(
                        "managed-registration-needs-directory",
                        409,
                    )
                    .into());
                }
                if connection.mode == "unconfigured" && !qr {
                    println!("{}", serde_json::to_string(&store.add_device()?)?);
                } else {
                    let uri = store.issue_registration()?;
                    let qr_text = if qr {
                        Some(
                            qrcode::QrCode::with_error_correction_level(
                                uri.as_bytes(),
                                qrcode::EcLevel::M,
                            )
                            .map_err(|_| {
                                risunest_sync_server::Error::new("registration-qr-too-large", 413)
                            })?
                            .render::<qrcode::render::unicode::Dense1x2>()
                            .quiet_zone(true)
                            .build(),
                        )
                    } else {
                        None
                    };
                    println!("{uri}");
                    if let Some(text) = qr_text {
                        println!("{text}");
                    }
                }
            }
            Some("revoke") => store.revoke_device(&revoke.unwrap())?,
            _ => return Err(USAGE.into()),
        },
        "connection" => {
            match subcommand.as_deref() {
                Some("configure") => store.configure_connection(ConnectionOptions {
                    endpoint,
                    cloudflared,
                    registry_url,
                })?,
                Some("status") => (),
                Some("repost") => store.request_republication()?,
                _ => return Err(USAGE.into()),
            }
            println!("{}", serde_json::to_string(&store.connection_status()?)?);
        }
        "serve" => {
            let listener = tokio::net::TcpListener::bind(config.listen).await?;
            let origin = listener.local_addr()?;
            let store = Arc::new(store);
            eprintln!(
                "sync listener ready: {} ({})",
                listener.local_addr()?,
                if config.https_proxy {
                    "HTTPS proxy origin"
                } else {
                    "local development"
                }
            );
            let workload = risunest_sync_server::workload::Workload::new();
            let management =
                Management::start_with_workload(store.clone(), origin, workload.clone()).await?;
            let mut stopped = management.shutdown_receiver();
            let result = axum::serve(listener, http::router_with_workload(store, workload))
                .with_graceful_shutdown(async move {
                    tokio::select! { _ = shutdown() => (), _ = stopped.changed() => () }
                })
                .await;
            management.close().await;
            result?;
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
