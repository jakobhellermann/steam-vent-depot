//! Print the depots and branches of an app.
//!
//! Usage: `cargo run --example list_depots -- <username> <password> <app_id>`
//!
//! Caches the refresh token in `~/.cache/steam-vent/refresh_tokens.json` so
//! subsequent runs skip Steam Guard.

use std::env::args;

use anyhow::{Context, Result};

use steam_vent_depot::DepotClient;

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

    let connection = login::establish_connection(&account, &password).await?;
    let depot = DepotClient::new(connection);

    let info = depot.app_info(app_id).await?;

    dbg!(&info);
    println!("app {}", info.app_id);

    if !info.depots.branches.is_empty() {
        println!("branches:");
        for (name, branch) in &info.depots.branches {
            println!("  {:<14} build {}", name, branch.build_id);
        }
    }

    println!("depots:");
    let mut depot_ids: Vec<_> = info.depots.depots.keys().copied().collect();
    depot_ids.sort();
    for id in depot_ids {
        let d = &info.depots.depots[&id];
        let os = d
            .config
            .as_ref()
            .and_then(|c| c.oslist.as_deref())
            .unwrap_or("-");
        if d.manifests.is_empty() {
            println!("  {id:>8}  os={os:<20} (no manifests)");
        } else {
            for (branch, m) in &d.manifests {
                println!(
                    "  {id:>8}  os={os:<20} branch={branch:<20} gid={:<20} size={}",
                    m.gid,
                    human_bytes(m.size),
                );
            }
        }
    }

    Ok(())
}

/// IEC binary prefixes (GiB/MiB/…), one decimal place. Steam usually rounds at
/// the depot level so values are big enough for this to read cleanly.
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
