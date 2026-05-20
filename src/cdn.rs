//! Content Delivery Network — Steam's content servers (`*.steamcontent.com`).
//!
//! These are the HTTPS hosts that serve depot manifests and chunks, separate
//! from the CM channel that handles auth and metadata. We discover them via
//! `ContentServerDirectory.GetServersForSteamPipe` and rank by load.

use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto::steammessages_contentsystem_steamclient::{
    CContentServerDirectory_GetServersForSteamPipe_Request, CContentServerDirectory_ServerInfo,
};

use crate::error::{DepotError, Result};

/// What kind of cache this content server is. Steam uses a few types with
/// slightly different semantics; we keep them as-is so callers can filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdnKind {
    /// Valve-operated CDN edge.
    Cdn,
    /// ISP-operated cache (e.g. LANCache-style).
    SteamCache,
    /// Generic HTTP cache.
    OpenCache,
    /// Anything we don't recognise; the string is preserved.
    Other(String),
}

impl CdnKind {
    fn from_wire(s: &str) -> Self {
        match s {
            "CDN" => CdnKind::Cdn,
            "SteamCache" => CdnKind::SteamCache,
            "OpenCache" => CdnKind::OpenCache,
            other => CdnKind::Other(other.into()),
        }
    }
}

/// Whether the content server accepts HTTPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpsSupport {
    /// Server requires HTTPS.
    Mandatory,
    /// Server accepts HTTPS but plain HTTP also works.
    Optional,
    /// HTTPS is not supported.
    None,
    /// Anything else Steam invents in the future — treated as "don't use".
    Unknown,
}

impl HttpsSupport {
    fn from_wire(s: &str) -> Self {
        match s {
            "mandatory" => HttpsSupport::Mandatory,
            "optional" => HttpsSupport::Optional,
            "none" => HttpsSupport::None,
            _ => HttpsSupport::Unknown,
        }
    }
}

/// One content server, the subset of fields we care about. Use [`Self::manifest_url`]
/// to build a download URL for a specific (depot, manifest, request code) tuple.
#[derive(Debug, Clone)]
pub struct CdnServer {
    pub kind: CdnKind,
    pub host: String,
    /// If set, this is the `Host:` header / SNI name to send when the host is
    /// behind a reverse proxy. Falls back to `host`.
    pub vhost: Option<String>,
    pub https: HttpsSupport,
    pub cell_id: Option<u32>,
    /// Higher = more loaded. Returned servers are ranked best-first by us.
    pub load: Option<i32>,
    pub weighted_load: Option<f32>,
    /// If non-empty, this server only serves these app ids.
    pub allowed_apps: Vec<u32>,
}

impl CdnServer {
    fn from_proto(s: CContentServerDirectory_ServerInfo) -> Self {
        let kind = CdnKind::from_wire(s.type_());
        let https = HttpsSupport::from_wire(s.https_support());
        CdnServer {
            kind,
            host: s.host.unwrap_or_default(),
            vhost: s.vhost,
            https,
            cell_id: s.cell_id.map(|c| c as u32),
            load: s.load,
            weighted_load: s.weighted_load,
            allowed_apps: s.allowed_app_ids,
        }
    }

    /// HTTPS base URL for this server (`https://<vhost-or-host>`).
    pub fn base_url(&self) -> String {
        let host = self.vhost.as_deref().unwrap_or(&self.host);
        format!("https://{host}")
    }

    /// Whether this server can serve the given app id. Servers with no
    /// `allowed_app_ids` filter serve everything.
    pub fn allows_app(&self, app_id: u32) -> bool {
        self.allowed_apps.is_empty() || self.allowed_apps.contains(&app_id)
    }

    /// Build the SteamPipe manifest-download URL for this server.
    ///
    /// `5` is the SteamKit `MANIFEST_VERSION` constant — the wire format
    /// version of the manifest binary served at this path.
    pub fn manifest_url(&self, depot_id: u32, manifest_id: u64, request_code: u64) -> String {
        format!(
            "{base}/depot/{depot_id}/manifest/{manifest_id}/5/{request_code}",
            base = self.base_url(),
        )
    }
}

/// Internal: fetch the list, filter to usable hosts, sort by weighted_load asc.
pub(crate) async fn fetch_cdn_servers(conn: &Connection, cell_id: u32) -> Result<Vec<CdnServer>> {
    let req = CContentServerDirectory_GetServersForSteamPipe_Request {
        cell_id: Some(cell_id),
        max_servers: Some(20),
        ..Default::default()
    };
    let resp = conn.service_method(req).await?;

    let mut servers: Vec<CdnServer> = resp
        .servers
        .into_iter()
        .map(CdnServer::from_proto)
        // Only HTTPS-capable hosts.
        .filter(|s| !matches!(s.https, HttpsSupport::None | HttpsSupport::Unknown))
        // Drop CN-only and weird types.
        .filter(|s| {
            matches!(
                s.kind,
                CdnKind::Cdn | CdnKind::SteamCache | CdnKind::OpenCache
            )
        })
        // Drop entries that came back without a host string.
        .filter(|s| !s.host.is_empty())
        .collect();

    servers.sort_by(|a, b| {
        a.weighted_load
            .unwrap_or(f32::INFINITY)
            .partial_cmp(&b.weighted_load.unwrap_or(f32::INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if servers.is_empty() {
        return Err(DepotError::NoCdnHosts);
    }
    Ok(servers)
}
