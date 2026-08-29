//! Full pipeline: log in, look up an app's depots, pick the manifest for one
//! depot+branch, decrypt + parse it, list a few files.
//!
//! Usage:
//!   cargo run --example fetch_manifest -- <user> <pw> <app_id> <depot_id> <branch>
//!
//! Example for Hollow Knight Silksong's Linux depot:
//!   cargo run --example fetch_manifest -- USER PW 1030300 1030303 public

use std::env::args;

use anyhow::{Context, Result};

use steam_vent_depot::{DepotClient, DepotFileKind};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = args().skip(1);
    let account = args.next().context("missing username")?;
    let password = args.next().context("missing password")?;
    let app_id: u32 = args
        .next()
        .context("missing app_id")?
        .parse()
        .context("app_id must be a number")?;
    let depot_id: u32 = args
        .next()
        .context("missing depot_id")?
        .parse()
        .context("depot_id must be a number")?;
    let branch = args.next().unwrap_or_else(|| "public".into());

    let connection = login::establish_connection(&account, &password).await?;
    let depot = DepotClient::new(connection);

    // Resolve manifest gid via app info, then run the full fetch pipeline.
    let info = depot.app_info(app_id).await?;
    let manifest_gid = info.manifest_gid(depot_id, &branch)?;
    println!("manifest gid for depot {depot_id} branch {branch}: {manifest_gid}");

    let depot_key = depot.depot_key(app_id, depot_id).await?;
    println!("got depot key ({} bytes)", depot_key.as_bytes().len());

    let cdn_servers = depot.cdn_servers().await?;
    println!(
        "{} CDN servers, best: {}",
        cdn_servers.len(),
        cdn_servers[0].host
    );

    let request_code = depot
        .manifest_request_code(app_id, depot_id, manifest_gid, &branch)
        .await?;
    println!("request code: {request_code}");

    let manifest = depot
        .fetch_manifest_with_code(&cdn_servers, depot_id, manifest_gid, request_code, &depot_key)
        .await?;

    println!(
        "\nmanifest depot={} id={} created={} files={} {} uncompressed",
        manifest.depot_id,
        manifest.manifest_id,
        manifest.creation_time,
        manifest.files.len(),
        human_bytes(manifest.size_uncompressed),
    );

    let limit = 100000;
    println!("\nfirst {limit} entries:");
    for f in manifest.files.iter().take(limit) {
        let kind = match f.kind {
            DepotFileKind::File { .. } => "F",
            DepotFileKind::Directory => "D",
            DepotFileKind::Symlink { .. } => "L",
        };
        println!(
            "  {kind} {:>10}  {}  (chunks: {})",
            human_bytes(f.size),
            f.path,
            f.chunks().len(),
        );
    }

    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[path = "support/login.rs"]
mod login;
