//! Discover Steam content servers (CDN/SteamCache hosts) for the current login
//! cell and list them sorted by load.
//!
//! Usage: `cargo run --example cdn -- <username> <password>`

use std::env::args;

use anyhow::{Context, Result};

use steam_vent_depot::DepotClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = args().skip(1);
    let account = args.next().context("missing username")?;
    let password = args.next().context("missing password")?;

    let connection = login::establish_connection(&account, &password).await?;
    let depot = DepotClient::new(connection);

    let servers = depot.cdn_servers().await?;
    println!("found {} usable CDN servers:", servers.len());
    for s in &servers {
        let host = s.vhost.as_deref().unwrap_or(&s.host);
        let load = s
            .weighted_load
            .map(|l| format!("{l:.0}"))
            .unwrap_or_else(|| "?".into());
        let scope = if s.allowed_apps.is_empty() {
            "all apps".into()
        } else {
            format!("apps={:?}", s.allowed_apps)
        };
        println!(
            "  load={load:>4}  cell={:<5} {:?} {host} [{scope}]",
            s.cell_id
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into()),
            s.kind,
        );
    }

    Ok(())
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
