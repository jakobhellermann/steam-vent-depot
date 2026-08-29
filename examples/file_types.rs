//! Aggregate file extensions across every owned game's public Windows manifests.
//!
//! Usage:
//!   cargo run --example file_types -- <username> <password> [top_n]
//!   cargo run --example file_types -- --report-only [top_n]
//!
//! Two phases:
//!  1. Fetch — log in, walk owned games, download each app's `public` Windows
//!     manifest(s), count file extensions per app, and write the per-app result
//!     to `file_types.json` incrementally so a crash/Ctrl-C doesn't lose work.
//!     Re-running skips apps already in the cache (both successful and failed).
//!  2. Report — load the JSON, aggregate, print a histogram with top apps per
//!     extension. `--report-only` skips the fetch phase entirely.
//!
//! Example output:
//!   png: 5123 (factorio: 1200, celeste: 800, ...)
//!   bank: 412 (celeste: 200, ...)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env::args;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use steam_vent::ConnectionTrait;
use steam_vent_proto::steammessages_player_steamclient::CPlayer_GetOwnedGames_Request;

use steam_vent_depot::{Depot, DepotClient, DepotFileKind};

/// Parallel apps processed simultaneously. Each app may fan out to several
/// depot manifest downloads internally; keep this low to stay polite to the CDN.
const APP_CONCURRENCY: usize = 4;

const CACHE_PATH: &str = "file_types.json";

/// On-disk shape. One entry per app we tried, successful or not.
#[derive(Serialize, Deserialize, Default)]
struct Cache {
    /// `app_id` as a string because JSON object keys must be strings.
    apps: BTreeMap<String, AppEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct AppEntry {
    name: String,
    #[serde(flatten)]
    result: AppResult,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "status", rename_all = "lowercase")]
enum AppResult {
    Ok {
        /// `ext -> file count`, deduplicated by path within the app.
        extensions: BTreeMap<String, u64>,
    },
    Err {
        error: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut raw: Vec<String> = args().skip(1).collect();
    let report_only = raw.first().map(|s| s == "--report-only").unwrap_or(false);
    if report_only {
        raw.remove(0);
    }

    let cache_path = PathBuf::from(CACHE_PATH);

    if !report_only {
        let mut it = raw.drain(..);
        let account = it.next().context("missing username")?;
        let password = it.next().context("missing password")?;
        let top_n = parse_top_n(it.next())?;
        drop(it);
        fetch_phase(&account, &password, &cache_path).await?;
        report_phase(&cache_path, top_n)?;
    } else {
        let top_n = parse_top_n(raw.drain(..).next())?;
        report_phase(&cache_path, top_n)?;
    }

    Ok(())
}

fn parse_top_n(s: Option<String>) -> Result<usize> {
    s.map(|s| s.parse().context("top_n must be a number"))
        .transpose()
        .map(|opt| opt.unwrap_or(5))
}

async fn fetch_phase(account: &str, password: &str, cache_path: &Path) -> Result<()> {
    let cache = Arc::new(Mutex::new(load_cache(cache_path)?));
    let already_done: HashSet<u32> = cache
        .lock()
        .await
        .apps
        .keys()
        .filter_map(|k| k.parse().ok())
        .collect();

    let connection = login::establish_connection(account, password).await?;
    let depot = Arc::new(DepotClient::new(connection.clone()));

    let owned = connection
        .service_method(CPlayer_GetOwnedGames_Request {
            steamid: Some(connection.steam_id().into()),
            include_appinfo: Some(true),
            include_played_free_games: Some(true),
            ..Default::default()
        })
        .await?;

    let pending: Vec<(u32, String)> = owned
        .games
        .iter()
        .map(|g| (g.appid() as u32, g.name().to_string()))
        .filter(|(id, _)| !already_done.contains(id))
        .collect();

    let total_owned = owned.games.len();
    let skipped = total_owned - pending.len();
    eprintln!(
        "owned={total_owned}  cached={skipped}  to fetch={}  (concurrency={APP_CONCURRENCY})",
        pending.len()
    );

    if pending.is_empty() {
        return Ok(());
    }

    let cdn_servers = Arc::new(depot.cdn_servers().await?);
    let to_fetch = pending.len();

    stream::iter(pending)
        .enumerate()
        .for_each_concurrent(APP_CONCURRENCY, |(idx, (app_id, name))| {
            let depot = depot.clone();
            let cdn_servers = cdn_servers.clone();
            let cache = cache.clone();
            let cache_path = cache_path.to_path_buf();
            async move {
                let result = match collect_app_extensions(&depot, &cdn_servers, app_id).await {
                    Ok(map) => {
                        eprintln!(
                            "[{:>4}/{to_fetch}] OK  app={app_id:<8} name={name:<40} exts={}",
                            idx + 1,
                            map.len()
                        );
                        AppResult::Ok {
                            extensions: map.into_iter().collect(),
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[{:>4}/{to_fetch}] ERR app={app_id:<8} name={name:<40} err={e}",
                            idx + 1
                        );
                        AppResult::Err {
                            error: e.to_string(),
                        }
                    }
                };

                let mut guard = cache.lock().await;
                guard.apps.insert(
                    app_id.to_string(),
                    AppEntry {
                        name: name.clone(),
                        result,
                    },
                );
                if let Err(e) = save_cache(&cache_path, &guard) {
                    eprintln!("warn: failed to persist cache: {e}");
                }
            }
        })
        .await;

    Ok(())
}

fn report_phase(cache_path: &Path, top_n: usize) -> Result<()> {
    let cache = load_cache(cache_path)?;
    if cache.apps.is_empty() {
        eprintln!("cache is empty — run a fetch first");
        return Ok(());
    }

    let mut ok_count = 0usize;
    let mut err_count = 0usize;
    let mut ext_totals: HashMap<String, u64> = HashMap::new();
    let mut ext_per_app: HashMap<String, Vec<(String, u64)>> = HashMap::new();
    for entry in cache.apps.values() {
        match &entry.result {
            AppResult::Ok { extensions } => {
                ok_count += 1;
                for (ext, count) in extensions {
                    *ext_totals.entry(ext.clone()).or_default() += count;
                    ext_per_app
                        .entry(ext.clone())
                        .or_default()
                        .push((entry.name.clone(), *count));
                }
            }
            AppResult::Err { .. } => err_count += 1,
        }
    }

    let mut ranked: Vec<(String, u64)> = ext_totals.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    println!(
        "\nfile extensions across {ok_count} apps ({err_count} failed, cache: {}):",
        cache_path.display()
    );
    for (ext, total) in &ranked {
        let apps = ext_per_app.get_mut(ext).expect("populated above");
        apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let top: Vec<String> = apps
            .iter()
            .take(top_n)
            .map(|(name, c)| format!("{name}: {c}"))
            .collect();
        println!("  {ext:<12} {total:>10}  ({})", top.join(", "));
    }

    Ok(())
}

fn load_cache(path: &Path) -> Result<Cache> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse cache at {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cache::default()),
        Err(e) => Err(e).context("failed to read cache"),
    }
}

fn save_cache(path: &Path, cache: &Cache) -> Result<()> {
    // Write atomically so an interrupted run doesn't corrupt the cache.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(cache)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Fetch every public-branch Windows manifest the app exposes and tally
/// extension counts. Files are deduplicated by path so the same file appearing
/// in multiple Windows-eligible depots is counted once.
async fn collect_app_extensions(
    depot: &DepotClient,
    cdn_servers: &[steam_vent_depot::CdnServer],
    app_id: u32,
) -> Result<HashMap<String, u64>> {
    let info = depot.app_info(app_id).await?;

    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut ext_counts: HashMap<String, u64> = HashMap::new();

    for (&depot_id, d) in &info.depots.depots {
        if d.depot_from_app.is_some() {
            continue;
        }
        if !is_windows_depot(d) {
            continue;
        }
        let Some(manifest_ref) = d.manifests.get("public") else {
            continue;
        };
        let manifest_gid = manifest_ref.gid;

        let depot_key = depot.depot_key(app_id, depot_id).await?;
        let manifest = depot
            .fetch_manifest(cdn_servers, app_id, depot_id, manifest_gid, "public", &depot_key)
            .await?;

        for f in &manifest.files {
            if !matches!(f.kind, DepotFileKind::File { .. }) {
                continue;
            }
            if !seen_paths.insert(f.path.clone()) {
                continue;
            }
            let ext = extension(&f.path);
            *ext_counts.entry(ext).or_default() += 1;
        }
    }

    Ok(ext_counts)
}

/// Steam treats a missing `oslist` as "all platforms" so we include those.
fn is_windows_depot(d: &Depot) -> bool {
    let Some(config) = d.config.as_ref() else {
        return true;
    };
    let Some(oslist) = config.oslist.as_deref() else {
        return true;
    };
    oslist
        .split(',')
        .any(|s| s.trim().eq_ignore_ascii_case("windows"))
}

/// Lowercased extension, or `"(none)"` for files without one. Uses the final
/// path component so something like `foo.bar/baz` is treated as having no
/// extension.
fn extension(path: &str) -> String {
    let basename = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path);
    match Path::new(basename).extension().and_then(|e| e.to_str()) {
        Some(e) if !e.is_empty() => e.to_ascii_lowercase(),
        _ => "(none)".into(),
    }
}

#[path = "support/login.rs"]
mod login;
