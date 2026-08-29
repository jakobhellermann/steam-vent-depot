//! Fetch `AppInfo` for every owned game, report which ones fail to parse.
//!
//! Usage: `cargo run --example all_app_infos -- <username> <password>`
//!
//! Prints "OK app=<id> name=<name>" or "ERR app=<id> err=<message>" per game.
//! Exit code 0 iff every app parsed.

use std::env::args;

use anyhow::{Context, Result};
use steam_vent::ConnectionTrait;
use steam_vent_proto::steammessages_player_steamclient::CPlayer_GetOwnedGames_Request;

use steam_vent_depot::DepotClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = args().skip(1);
    let account = args.next().context("missing username")?;
    let password = args.next().context("missing password")?;

    let connection = login::establish_connection(&account, &password).await?;
    let depot = DepotClient::new(connection.clone());

    let owned = connection
        .service_method(CPlayer_GetOwnedGames_Request {
            steamid: Some(connection.steam_id().into()),
            include_appinfo: Some(true),
            include_played_free_games: Some(true),
            ..Default::default()
        })
        .await?;

    let mut failed = 0u32;
    let mut total = 0u32;
    for g in owned.games {
        total += 1;
        let appid = g.appid();
        let name = g.name().to_string();
        match depot.app_info(appid as u32).await {
            Ok(info) => println!("OK  app={appid:<8} name={}", info.common.name),
            Err(e) => {
                failed += 1;
                println!("ERR app={appid:<8} name={name:<40} err={e}");
            }
        }
    }

    eprintln!("\n{failed}/{total} apps failed to parse");
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[path = "support/login.rs"]
mod login;
