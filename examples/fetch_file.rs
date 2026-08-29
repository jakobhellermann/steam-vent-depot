//! Full pipeline: log in, fetch a manifest, find one file in it, download
//! all its chunks, decrypt + decompress + verify, write to disk.
//!
//! Usage:
//!   cargo run --example fetch_file -- <user> <pw> <app_id> <depot_id> <branch> <path>
//!
//! Example:
//!   cargo run --example fetch_file -- USER PW 1030300 1030303 public \
//!     'Hollow Knight Silksong_Data/StreamingAssets/aa/StandaloneLinux64/sfxstatic_assets_areashellwood.bundle'

use std::env::args;
use std::path::PathBuf;

use anyhow::{Context, Result};

use steam_vent_depot::{DepotClient, DepotError, DepotFileKind};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = args().skip(1);
    let account = args.next().context("missing username")?;
    let password = args.next().context("missing password")?;
    let app_id: u32 = args.next().context("missing app_id")?.parse()?;
    let depot_id: u32 = args.next().context("missing depot_id")?.parse()?;
    let branch = args.next().context("missing branch")?;
    let path = args.next().context("missing file path")?;

    let connection = login::establish_connection(&account, &password).await?;
    let depot = DepotClient::new(connection);

    let info = depot.app_info(app_id).await?;
    let manifest_gid = info.manifest_gid(depot_id, &branch)?;
    let depot_key = depot.depot_key(app_id, depot_id).await?;
    let cdn_servers = depot.cdn_servers().await?;
    let request_code = depot
        .manifest_request_code(app_id, depot_id, manifest_gid, &branch)
        .await?;
    let manifest = depot
        .fetch_manifest_with_code(
            &cdn_servers,
            depot_id,
            manifest_gid,
            request_code,
            &depot_key,
        )
        .await?;
    eprintln!(
        "manifest {} loaded ({} files, {} bytes uncompressed)",
        manifest.manifest_id,
        manifest.files.len(),
        manifest.size_uncompressed
    );

    let file = manifest
        .find_file(&path)
        .ok_or_else(|| DepotError::FileNotFound(path.clone()))?;
    if !matches!(file.kind, DepotFileKind::File { .. }) {
        anyhow::bail!("{path:?} is a {:?}, not a regular file", file.kind);
    }
    eprintln!(
        "found {path:?}: {} bytes in {} chunks",
        file.size,
        file.chunks().len()
    );

    // Reassemble into a buffer in chunk-offset order. The manifest typically lists
    // chunks already in offset order, but be defensive.
    let mut sorted = file.chunks().to_vec();
    sorted.sort_by_key(|c| c.offset);

    let mut buf = vec![0u8; file.size as usize];
    for (i, chunk) in sorted.iter().enumerate() {
        let data = depot
            .fetch_chunk(&cdn_servers, depot_id, chunk, &depot_key)
            .await?;
        let start = chunk.offset as usize;
        let end = start + data.len();
        buf[start..end].copy_from_slice(&data);
        eprintln!(
            "  chunk {}/{}: {} bytes (offset {start})",
            i + 1,
            sorted.len(),
            data.len()
        );
    }

    // Write to <basename> in current directory.
    let out_name = PathBuf::from(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out.bin".into());
    std::fs::write(&out_name, &buf)?;
    eprintln!("wrote {} bytes to {out_name}", buf.len());

    Ok(())
}

#[path = "support/login.rs"]
mod login;
