//! Shared login helper for examples. Caches the refresh token at
//! `~/.cache/steam-vent/refresh_tokens.json` so subsequent runs skip Steam Guard.

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
    let server_list = ServerList::discover_with(DiscoverOptions::default().with_cell(4)).await?;

    let connection = match load_refresh_token(account) {
        Some(token) => match Connection::access(&server_list, account, &token).await {
            Ok(conn) => {
                eprintln!("logged in with cached refresh token");
                conn
            }
            Err(e) => {
                eprintln!("cached refresh token rejected ({e}), falling back to password login");
                password_login(&server_list, account, password).await?
            }
        },
        None => password_login(&server_list, account, password).await?,
    };

    Ok(connection)
}
