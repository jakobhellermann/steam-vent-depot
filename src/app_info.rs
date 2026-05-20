//! App info — depots, branches, manifest pointers, and the rest of an app's metadata.
//!
//! Steam stores this as a VDF (Valve Data Format) tree per app. We fetch it
//! over the connection and deserialise just the depot-relevant parts.
//!
//! Omitted from the typed view (still present in the wire VDF):
//! - `common` — store metadata: name, type, icons, languages, OS lists, tags, …
//! - `extended` — publisher, developer, homepage, EULA URL, demo relationships
//! - `config` — launch configs, install scripts, cloud-save quotas, dependencies
//! - `ufs` — cloud-save path/quota definitions
//! - misc `depots` siblings like `workshopdepot`, `overridescddb`, `baselanguages`
//!
//! Internally this is fetched via Steam's "PICS" (Product Info Cache) service;
//! you can ignore that name unless you're debugging the wire protocol.

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto::steammessages_clientserver_appinfo::{
    CMsgClientPICSProductInfoRequest, CMsgClientPICSProductInfoResponse,
    cmsg_client_picsproduct_info_request,
};

use crate::error::{DepotError, Result};

/// Metadata for one app: id, depots, branches, manifest pointers.
#[derive(Deserialize, Debug, Clone)]
pub struct AppInfo {
    #[serde(rename = "appid")]
    pub app_id: u32,
    pub depots: DepotInfos,
}

impl AppInfo {
    pub(crate) async fn fetch(conn: &Connection, app_id: u32) -> Result<Self> {
        let req = CMsgClientPICSProductInfoRequest {
            apps: vec![cmsg_client_picsproduct_info_request::AppInfo {
                appid: Some(app_id),
                ..Default::default()
            }],
            meta_data_only: Some(false),
            single_response: Some(true),
            ..Default::default()
        };

        let resp: CMsgClientPICSProductInfoResponse = conn.job(req).await?;
        let app = resp
            .apps
            .into_iter()
            .next()
            .ok_or(DepotError::AppNotFound(app_id))?;
        let buffer = app.buffer.unwrap_or_default();
        // The VDF buffer is NUL-terminated; the parser doesn't accept that.
        let vdf = std::str::from_utf8(&buffer)?.trim_end_matches('\0');

        let envelope: AppInfoEnvelope = vdf_reader::from_str(vdf)?;
        Ok(envelope.appinfo)
    }

    /// Look up the manifest gid for a depot+branch combination.
    pub fn manifest_gid(&self, depot_id: u32, branch: &str) -> Result<u64> {
        let depot = self
            .depots
            .depots
            .get(&depot_id)
            .ok_or(DepotError::DepotNotFound {
                app: self.app_id,
                depot: depot_id,
            })?;
        depot
            .manifests
            .get(branch)
            .map(|m| m.gid)
            .ok_or_else(|| DepotError::BranchNotFound {
                app: self.app_id,
                depot: depot_id,
                branch: branch.into(),
            })
    }
}

/// The VDF wraps everything in a single `appinfo` key; we deserialise into
/// this and unwrap.
#[derive(Deserialize)]
struct AppInfoEnvelope {
    appinfo: AppInfo,
}

/// The `depots` section of the app VDF.
///
/// Mixes numeric depot ids with a few named entries; we route the numeric ones
/// into `depots` and recognise the named ones explicitly. Anything we don't
/// know is silently dropped.
#[derive(Debug, Clone, Default)]
pub struct DepotInfos {
    /// Each numeric depot id and its config/manifests.
    pub depots: HashMap<u32, Depot>,
    /// Build state per branch (`public`, `public-beta`, …).
    pub branches: HashMap<String, Branch>,
    /// `true` if the app has private branches the current account can't see.
    pub private_branches: bool,
}

impl<'de> Deserialize<'de> for DepotInfos {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};

        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = DepotInfos;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a depots VDF table")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<DepotInfos, M::Error> {
                let mut out = DepotInfos::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "branches" => {
                            out.branches = map.next_value()?;
                        }
                        "privatebranches" => {
                            // VDF only knows strings; "1" is true, anything else false.
                            let v: String = map.next_value()?;
                            out.private_branches = v == "1";
                        }
                        other => {
                            if let Ok(id) = other.parse::<u32>() {
                                let depot: Depot = map.next_value()?;
                                out.depots.insert(id, depot);
                            } else {
                                // Skip unknown named entries (workshopdepot, overridescddb, …).
                                let _: serde::de::IgnoredAny = map.next_value()?;
                            }
                        }
                    }
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(V)
    }
}

/// One depot. Most fields are optional because Steam reuses this shape for
/// "real" depots, shared/DLC depots, and workshop depots.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct Depot {
    /// Per-OS filter (`oslist = "windows,linux"`, `osarch = "64"`, …).
    pub config: Option<DepotConfig>,
    /// Branch name → manifest. Empty for shared/system depots that don't ship
    /// their own content.
    pub manifests: HashMap<String, ManifestRef>,
    /// `"1"` for depots Steam auto-generates (workshop bundles, redists, …).
    #[serde(rename = "systemdefined", deserialize_with = "de_vdf_bool", default)]
    pub system_defined: bool,
    /// When set, this depot's bits live in another app and are mounted from there.
    #[serde(rename = "depotfromapp")]
    pub depot_from_app: Option<u32>,
    /// `"1"` if Steam treats the install as shared with other apps.
    #[serde(rename = "sharedinstall", deserialize_with = "de_vdf_bool", default)]
    pub shared_install: bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DepotConfig {
    /// Comma-separated `windows,macos,linux` list this depot is built for.
    pub oslist: Option<String>,
    /// `"32"` or `"64"`.
    pub osarch: Option<String>,
    /// Languages this depot is restricted to.
    pub language: Option<String>,
}

/// Pointer to a depot manifest, as listed under `depots.<id>.manifests.<branch>`.
/// Not the manifest content itself — see [`crate::manifest::Manifest`] for that.
#[derive(Deserialize, Debug, Clone)]
pub struct ManifestRef {
    /// Manifest gid (the "id" used by `ContentServerDirectory.GetManifestRequestCode`).
    pub gid: u64,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// Compressed download size in bytes.
    pub download: u64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Branch {
    #[serde(rename = "buildid")]
    pub build_id: u64,
    #[serde(rename = "timeupdated", default)]
    pub time_updated: Option<u64>,
    #[serde(rename = "timebuildupdated", default)]
    pub time_build_updated: Option<u64>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Steam encodes bools as `"0"` or `"1"`. Absent means false (handled by `#[serde(default)]`
/// on the field). Any other value is an error so we notice if Valve changes the schema.
fn de_vdf_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = String::deserialize(d)?;
    match s.as_str() {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(serde::de::Error::custom(format!(
            "expected VDF bool \"0\" or \"1\", got {other:?}"
        ))),
    }
}
