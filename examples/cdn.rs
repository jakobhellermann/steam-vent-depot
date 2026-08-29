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

#[path = "support/login.rs"]
mod login;
