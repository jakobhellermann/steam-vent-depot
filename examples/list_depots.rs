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
        let dirs = ProjectDirs::from("", "steam-vent", "steam-vent")
            .expect("user cache not supported on this platform");
        dirs.cache_dir().join("refresh_tokens.json")
    }

    fn load_refresh_token(account: &str) -> Option<String> {
        let raw = fs::read_to_string(refresh_token_path()).ok()?;
        let map: HashMap<String, String> = serde_json::from_str(&raw).ok()?;
        map.get(account).cloned().filter(|t| !t.is_empty())
    }

    fn save_refresh_token(account: &str, token: &str) -> Result<()> {
        let path = refresh_token_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut map: HashMap<String, String> = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        map.insert(account.into(), token.into());
        fs::write(&path, serde_json::to_string(&map)?)?;
        Ok(())
    }

    async fn password_login(
        server_list: &ServerList,
        account: &str,
        password: &str,
    ) -> Result<Connection> {
        let conn = Connection::login(
            server_list,
            account,
            password,
            FileGuardDataStore::user_cache(),
            ConsoleAuthConfirmationHandler::default().or(DeviceConfirmationHandler),
        )
        .await?;
        if let Some(t) = conn.access_token() {
            save_refresh_token(account, t)?;
            eprintln!("saved refresh token");
        }
        Ok(conn)
    }

    pub async fn establish_connection(account: &str, password: &str) -> Result<Connection> {
        let server_list =
            ServerList::discover_with(DiscoverOptions::default().with_cell(4)).await?;

        let connection = match load_refresh_token(account) {
            Some(token) => match Connection::access(&server_list, account, &token).await {
                Ok(conn) => {
                    eprintln!("logged in with cached refresh token");
                    conn
                }
                Err(e) => {
                    eprintln!(
                        "cached refresh token rejected ({e}), falling back to password login"
                    );
                    password_login(&server_list, account, password).await?
                }
            },
            None => password_login(&server_list, account, password).await?,
        };

        Ok(connection)
    }
}
