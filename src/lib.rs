//! Steam depot manifest and CDN client built on top of [`steam_vent`].
//!
//! Wraps the four pieces needed to discover what's in a Steam depot:
//! - app metadata (depots, branches, manifest gids)
//! - per-depot AES-256 decryption keys
//! - one-shot manifest request codes
//! - the HTTPS CDN servers serving manifests and chunks
//!
//! Then downloads, unzips and parses the manifest binary, decrypting filenames
//! along the way.

mod app_info;
mod cdn;
mod chunk;
mod error;
mod manifest;

pub use app_info::{AppInfo, Branch, Depot, DepotConfig, DepotInfos, ManifestRef};
pub use cdn::{CdnKind, CdnServer, HttpsSupport};
pub use error::{DepotError, Result};
pub use manifest::{Chunk, ChunkHash, DepotFile, DepotKey, FileKind, Manifest};

use steam_vent::Connection;

/// High-level wrapper around steam-vent's [`Connection`] for talking to Steam's
/// depot/content system.
#[derive(Clone)]
pub struct DepotClient {
    connection: Connection,
    http: reqwest::Client,
}

impl DepotClient {
    /// Create a depot client from an authenticated [`Connection`].
    ///
    /// The built-in HTTP client has a 10 s connect timeout and a 30 s
    /// total-request timeout — a single stalled CDN host shouldn't be
    /// allowed to occupy a parallel slot indefinitely. Use
    /// [`with_http`](Self::with_http) to override.
    pub fn new(connection: Connection) -> Self {
        // HTTP/2 negotiation via ALPN is already on if reqwest's
        // `http2` cargo feature is enabled (see this crate's
        // Cargo.toml). With H/2 the CDN can multiplex many in-flight
        // chunk requests onto a single TCP+TLS connection per host.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("default reqwest client builder shouldn't fail");
        DepotClient { connection, http }
    }

    /// Create a depot client with a custom [`reqwest::Client`] (useful for sharing
    /// a connection pool or setting custom timeouts).
    pub fn with_http(connection: Connection, http: reqwest::Client) -> Self {
        DepotClient { connection, http }
    }

    /// The underlying steam-vent connection.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Fetch metadata (depots, branches, manifest gids, …) for an app.
    pub async fn app_info(&self, app_id: u32) -> Result<AppInfo> {
        AppInfo::fetch(&self.connection, app_id).await
    }

    /// Discover content-delivery servers ranked best-first by load. Uses the
    /// session's cell id from login.
    pub async fn cdn_servers(&self) -> Result<Vec<CdnServer>> {
        cdn::fetch_cdn_servers(&self.connection, self.connection.cell_id()).await
    }

    /// Like [`cdn_servers`](Self::cdn_servers) but for a specific cell id.
    pub async fn cdn_servers_for_cell(&self, cell_id: u32) -> Result<Vec<CdnServer>> {
        cdn::fetch_cdn_servers(&self.connection, cell_id).await
    }

    /// Fetch the AES-256 decryption key for a depot. Constant per depot.
    pub async fn depot_key(&self, app_id: u32, depot_id: u32) -> Result<DepotKey> {
        manifest::fetch_depot_key(&self.connection, app_id, depot_id).await
    }

    /// Get a one-shot request code authorising a single manifest download.
    /// Codes are short-lived; call this just before [`fetch_manifest`](Self::fetch_manifest).
    pub async fn manifest_request_code(
        &self,
        app_id: u32,
        depot_id: u32,
        manifest_id: u64,
        branch: &str,
    ) -> Result<u64> {
        manifest::fetch_manifest_request_code(
            &self.connection,
            app_id,
            depot_id,
            manifest_id,
            branch,
        )
        .await
    }

    /// Download a single chunk: HTTP GET, AES-decrypt, decompress (VZip/LZMA),
    /// verify Adler-32. Returns the plaintext chunk bytes.
    pub async fn fetch_chunk(
        &self,
        cdn_servers: &[CdnServer],
        depot_id: u32,
        chunk: &Chunk,
        depot_key: &DepotKey,
    ) -> Result<Vec<u8>> {
        chunk::fetch_chunk(&self.http, cdn_servers, depot_id, chunk, depot_key).await
    }

    /// Download a manifest from a CDN host, decrypt filenames, return the
    /// parsed file list. Tries hosts in order until one succeeds.
    pub async fn fetch_manifest(
        &self,
        cdn_servers: &[CdnServer],
        depot_id: u32,
        manifest_id: u64,
        request_code: u64,
        depot_key: &DepotKey,
    ) -> Result<Manifest> {
        manifest::fetch_manifest(
            &self.http,
            cdn_servers,
            depot_id,
            manifest_id,
            request_code,
            depot_key,
        )
        .await
    }
}
