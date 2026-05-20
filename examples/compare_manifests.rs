//! Compare N manifests of the same depot and report chunk reuse.
//!
//! Chunks are content-addressed by SHA-1, so the same sha across two manifests
//! means the bytes are byte-identical and don't need to be re-downloaded.
//!
//! Usage:
//!   cargo run --example compare_manifests -- <user> <pw> <app_id> <depot_id> <gid>[:<branch>] ...
//!
//! Branch defaults to "public" if omitted. Use `GID:branchname` for manifests
//! that belong to a restricted/beta branch — Steam returns AccessDenied
//! otherwise.
//!
//! Example (mixing public and public-beta):
//!   cargo run --example compare_manifests -- USER PW 1030300 1030303 \
//!       7921642076658611197 3678462974375346661:public-beta

use std::collections::{HashMap, HashSet};
use std::env::args;

use anyhow::{Context, Result};

use futures_util::future::try_join_all;
use jiff::Timestamp;
use steam_vent_depot::{ChunkHash, DepotClient, Manifest};

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
    let entries: Vec<(u64, String)> = args
        .map(|a| {
            let (gid_str, branch) = match a.split_once(':') {
                Some((g, b)) => (g.to_string(), b.to_string()),
                None => (a, "public".to_string()),
            };
            let gid: u64 = gid_str.parse().context("manifest gid must be a number")?;
            Ok((gid, branch))
        })
        .collect::<Result<_>>()?;
    if entries.len() < 2 {
        anyhow::bail!("need at least two manifest gids to compare");
    }

    let connection = login::establish_connection(&account, &password).await?;
    let depot = DepotClient::new(connection);

    let depot_key = depot.depot_key(app_id, depot_id).await?;
    let cdn_servers = depot.cdn_servers().await?;

    let mut manifests: Vec<Manifest> = try_join_all(entries.iter().map(|(gid, branch)| {
        let depot = &depot;
        let cdn_servers = &cdn_servers;
        let depot_key = &depot_key;
        async move {
            eprintln!("fetching manifest {gid} (branch {branch})");
            let code = depot
                .manifest_request_code(app_id, depot_id, *gid, branch)
                .await?;
            let m = depot
                .fetch_manifest(cdn_servers, depot_id, *gid, code, depot_key)
                .await?;
            eprintln!("fetched manifest {gid}");
            Ok::<_, anyhow::Error>(m)
        }
    }))
    .await?;
    manifests.sort_by_key(|m| m.creation_time);

    // Per-manifest chunk set (sha -> compressed size). HashMap dedups
    // chunks that appear multiple times *within* a single manifest
    // (e.g. an empty file's chunk reused across many files).
    let chunk_sets: Vec<HashMap<ChunkHash, u32>> = manifests
        .iter()
        .map(|m| {
            let mut map = HashMap::new();
            for f in &m.files {
                for c in &f.chunks {
                    map.insert(c.sha, c.size_compressed);
                }
            }
            map
        })
        .collect();

    println!("\n== per-manifest ==");
    println!(
        "{:<22} {:<20} {:>10} {:>14} {:>14}",
        "manifest", "created", "files", "unique chunks", "compressed"
    );
    for (m, chunks) in manifests.iter().zip(&chunk_sets) {
        let compressed: u64 = chunks.values().map(|&s| s as u64).sum();
        println!(
            "{:<22} {:<20} {:>10} {:>14} {:>14}",
            m.manifest_id,
            format_ts(m.creation_time),
            m.files.len(),
            chunks.len(),
            human_bytes(compressed),
        );
    }

    // Union across all manifests = bytes you'd download if you had nothing
    // and wanted every version. Naive sum = bytes if each version were
    // fetched independently with no reuse.
    let mut union: HashMap<ChunkHash, u32> = HashMap::new();
    for chunks in &chunk_sets {
        for (sha, &size) in chunks {
            union.insert(*sha, size);
        }
    }
    let naive: u64 = chunk_sets
        .iter()
        .map(|c| c.values().map(|&s| s as u64).sum::<u64>())
        .sum();
    let deduped: u64 = union.values().map(|&s| s as u64).sum();

    println!("\n== union across all {} manifests ==", manifests.len());
    println!("unique chunks:      {}", union.len());
    println!("naive total:        {}", human_bytes(naive));
    println!("deduped total:      {}", human_bytes(deduped));
    println!(
        "savings:            {} ({:.1}%)",
        human_bytes(naive.saturating_sub(deduped)),
        if naive > 0 {
            (naive - deduped) as f64 / naive as f64 * 100.0
        } else {
            0.0
        }
    );

    // Incremental: walk manifests oldest-to-newest, accumulate chunks,
    // report how much each one adds on top of the running union.
    println!("\n== incremental ==");
    println!(
        "{:<22} {:>14} {:>14} {:>14} {:>14}",
        "manifest", "new chunks", "new bytes", "total chunks", "total bytes"
    );
    let mut seen: HashSet<ChunkHash> = HashSet::new();
    let mut cum_bytes: u64 = 0;
    for (m, chunks) in manifests.iter().zip(&chunk_sets) {
        let mut new_chunks = 0usize;
        let mut new_bytes = 0u64;
        for (sha, &size) in chunks {
            if seen.insert(*sha) {
                new_chunks += 1;
                new_bytes += size as u64;
            }
        }
        cum_bytes += new_bytes;
        println!(
            "{:<22} {:>14} {:>14} {:>14} {:>14}",
            m.manifest_id,
            new_chunks,
            human_bytes(new_bytes),
            seen.len(),
            human_bytes(cum_bytes),
        );
    }

    Ok(())
}

fn format_ts(unix: u32) -> String {
    Timestamp::from_second(unix as i64)
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|_| unix.to_string())
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

mod login {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    use anyhow::Result;
    use directories::ProjectDirs;
    use steam_vent::auth::{
        AuthConfirmationHandler, ConsoleAuthConfirmationHandler, DeviceConfirmationHandler,
        FileGuardDataStore,
    };
    use steam_vent::{Connection, DiscoverOptions, ServerList};

    fn refresh_token_path() -> PathBuf {
        ProjectDirs::from("", "steam-vent", "steam-vent")
            .expect("no cache dir")
            .cache_dir()
            .join("refresh_tokens.json")
    }
    fn load(account: &str) -> Option<String> {
        let raw = fs::read_to_string(refresh_token_path()).ok()?;
        let map: HashMap<String, String> = serde_json::from_str(&raw).ok()?;
        map.get(account).cloned().filter(|t| !t.is_empty())
    }
    fn save(account: &str, token: &str) -> Result<()> {
        let path = refresh_token_path();
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let mut map: HashMap<String, String> = fs::read_to_string(&path)
            .ok()
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_default();
        map.insert(account.into(), token.into());
        fs::write(&path, serde_json::to_string(&map)?)?;
        Ok(())
    }
    pub async fn establish_connection(account: &str, password: &str) -> Result<Connection> {
        let server_list =
            ServerList::discover_with(DiscoverOptions::default().with_cell(4)).await?;
        if let Some(t) = load(account)
            && let Ok(c) = Connection::access(&server_list, account, &t).await
        {
            return Ok(c);
        }
        let c = Connection::login(
            &server_list,
            account,
            password,
            FileGuardDataStore::user_cache(),
            ConsoleAuthConfirmationHandler::default().or(DeviceConfirmationHandler),
        )
        .await?;
        if let Some(t) = c.access_token() {
            save(account, t)?;
        }
        Ok(c)
    }
}
