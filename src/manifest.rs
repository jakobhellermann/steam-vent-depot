//! Depot manifest: download, decrypt, parse.
//!
//! A depot manifest is the per-version index of every file in a depot:
//! path, size, SHA-1, and the list of 1 MiB content-addressed chunks that
//! the actual file bytes are split into. With one in hand you can do
//! incremental updates, hash verification, or build a custom downloader.
//!
//! The CDN serves manifests as a single-entry ZIP containing a binary blob
//! of magic-prefixed protobuf sections. Filenames inside the payload are
//! AES-256 encrypted with the depot key (separately from the file contents
//! on disk — depots are encrypted at the chunk level, not at the manifest
//! level).

use std::io::Read;

use aes::Aes256;
use aes::cipher::{
    Array, BlockCipherDecrypt, BlockModeDecrypt, KeyInit, KeyIvInit, block_padding::Pkcs7,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use steam_vent::{Connection, ConnectionTrait, EResult};
use steam_vent_proto::content_manifest::{
    ContentManifestMetadata, ContentManifestPayload, ContentManifestSignature,
};
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto::steammessages_contentsystem_steamclient::CContentServerDirectory_GetManifestRequestCode_Request;
use steam_vent_proto_common::protobuf::Message;

use crate::cdn::CdnServer;
use crate::error::{DepotError, Result};

/// AES-256 key for a depot. Constant per depot, doesn't change with manifests.
#[derive(Debug, Clone)]
pub struct DepotKey(pub [u8; 32]);

impl DepotKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl TryFrom<Vec<u8>> for DepotKey {
    type Error = DepotError;
    fn try_from(v: Vec<u8>) -> Result<Self> {
        let arr: [u8; 32] = v
            .try_into()
            .map_err(|v: Vec<u8>| DepotError::DepotKeyLength(v.len()))?;
        Ok(DepotKey(arr))
    }
}

/// Parsed depot manifest: file list + metadata.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub depot_id: u32,
    pub manifest_id: u64,
    /// Unix timestamp.
    pub creation_time: u32,
    pub size_uncompressed: u64,
    pub size_compressed: u64,
    pub files: Vec<DepotFile>,
}

impl Manifest {
    /// Find a file by exact path match (forward slashes). Returns `None` if absent.
    pub fn find_file(&self, path: &str) -> Option<&DepotFile> {
        self.files.iter().find(|f| f.path == path)
    }

    /// Lists all non-symlink file paths
    pub fn normal_paths(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .filter(|file| file.is_file())
            .map(|x| x.path.as_str())
    }
}

/// One entry in a depot manifest.
#[derive(Debug, Clone)]
pub struct DepotFile {
    pub path: String,
    pub size: u64,
    pub kind: DepotFileKind,
}

#[derive(Debug, Clone)]
pub enum DepotFileKind {
    /// Chunks are sorted by `offset` ascending and do not overlap.
    File {
        sha: FileHash,
        executable: bool,
        chunks: Vec<Chunk>,
    },
    Directory,
    Symlink {
        target: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Symlink,
}

impl DepotFile {
    pub fn file_type(&self) -> FileType {
        match &self.kind {
            DepotFileKind::File { .. } => FileType::File,
            DepotFileKind::Directory => FileType::Directory,
            DepotFileKind::Symlink { .. } => FileType::Symlink,
        }
    }

    pub fn sha(&self) -> Option<FileHash> {
        match &self.kind {
            DepotFileKind::File { sha, .. } => Some(*sha),
            _ => None,
        }
    }

    pub fn chunks(&self) -> &[Chunk] {
        match &self.kind {
            DepotFileKind::File { chunks, .. } => chunks,
            _ => &[],
        }
    }

    pub fn executable(&self) -> bool {
        matches!(&self.kind, DepotFileKind::File { executable: true, .. })
    }

    pub fn linktarget(&self) -> Option<&str> {
        match &self.kind {
            DepotFileKind::Symlink { target } => Some(target),
            _ => None,
        }
    }

    pub fn is_file(&self) -> bool {
        matches!(self.kind, DepotFileKind::File { .. })
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, DepotFileKind::Directory)
    }

    pub fn is_symlink(&self) -> bool {
        matches!(self.kind, DepotFileKind::Symlink { .. })
    }
}

macro_rules! sha1_hex_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(pub [u8; 20]);

        impl $name {
            /// Parse a 40-char lowercase or uppercase hex string. Returns `None`
            /// for any other length or non-hex byte.
            pub fn from_hex(s: &str) -> Option<Self> {
                if s.len() != 40 {
                    return None;
                }
                let b = s.as_bytes();
                let mut bytes = [0u8; 20];
                for i in 0..20 {
                    bytes[i] = (hex_nibble(b[i * 2])? << 4) | hex_nibble(b[i * 2 + 1])?;
                }
                Some($name(bytes))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                for b in &self.0 {
                    write!(f, "{:02x}", b)?;
                }
                Ok(())
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({self})", stringify!($name))
            }
        }
    };
}

sha1_hex_newtype!(
    /// SHA-1 of a chunk's plaintext content. Also the chunk's address on Steam's CDN.
    ChunkHash
);
sha1_hex_newtype!(
    /// SHA-1 of a file's full content
    FileHash
);

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Steam splits files into ~1 MiB chunks; each chunk is content-addressed
/// by its SHA-1 and stored separately on the CDN.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// SHA-1 of the *plaintext* chunk content. This is also the CDN object id
    /// (`/depot/<depot>/chunk/<sha-hex>`).
    pub sha: ChunkHash,
    /// Adler-32 of the plaintext chunk content, checked after decrypt+decompress.
    pub crc: u32,
    pub offset: u64,
    pub size_uncompressed: u32,
    pub size_compressed: u32,
}

const FLAG_EXECUTABLE: u32 = 0x20;
const FLAG_DIRECTORY: u32 = 0x40;
const FLAG_SYMLINK: u32 = 0x200;

// Magic prefixes for the four section kinds in the manifest binary,
// taken from SteamKit `DepotManifest.cs`.
const MAGIC_PAYLOAD: u32 = 0x71F617D0;
const MAGIC_METADATA: u32 = 0x1F4812BE;
const MAGIC_SIGNATURE: u32 = 0x1B81B817;
const MAGIC_END: u32 = 0x32C415AB;

/// Step 3 — fetch the AES-256 decryption key for a depot.
pub(crate) async fn fetch_depot_key(
    conn: &Connection,
    app_id: u32,
    depot_id: u32,
) -> Result<DepotKey> {
    let req = CMsgClientGetDepotDecryptionKey {
        app_id: Some(app_id),
        depot_id: Some(depot_id),
        ..Default::default()
    };
    let resp: CMsgClientGetDepotDecryptionKeyResponse = conn.job(req).await?;
    let eresult = EResult::try_from(resp.eresult())
        .map_err(|_| DepotError::ResponseMalformed("unknown EResult on GetDepotDecryptionKey"))?;
    if !matches!(eresult, EResult::OK) {
        return Err(DepotError::Steam(eresult));
    }
    let key_bytes = resp
        .depot_encryption_key
        .ok_or(DepotError::ResponseMalformed(
            "depot key response had no key",
        ))?;
    DepotKey::try_from(key_bytes)
}

/// Step 4 — one-shot code authorising a single manifest download.
pub(crate) async fn fetch_manifest_request_code(
    conn: &Connection,
    app_id: u32,
    depot_id: u32,
    manifest_id: u64,
    branch: &str,
) -> Result<u64> {
    let req = CContentServerDirectory_GetManifestRequestCode_Request {
        app_id: Some(app_id),
        depot_id: Some(depot_id),
        manifest_id: Some(manifest_id),
        app_branch: Some(branch.into()),
        ..Default::default()
    };
    let resp = conn.service_method(req).await?;
    resp.manifest_request_code
        .ok_or(DepotError::ResponseMalformed(
            "manifest_request_code missing",
        ))
}

/// Steps 6 + 7 — try each CDN host in turn, download the manifest blob,
/// then parse and decrypt filenames with `depot_key`.
pub(crate) async fn fetch_manifest(
    http: &reqwest::Client,
    cdn_servers: &[CdnServer],
    depot_id: u32,
    manifest_id: u64,
    request_code: u64,
    depot_key: &DepotKey,
) -> Result<Manifest> {
    if cdn_servers.is_empty() {
        return Err(DepotError::NoCdnHosts);
    }

    let mut last_err: Option<String> = None;
    for server in cdn_servers {
        let url = server.manifest_url(depot_id, manifest_id, request_code);
        let raw = match http
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => match resp.bytes().await {
                Ok(b) => b.to_vec(),
                Err(e) => {
                    last_err = Some(format!("{}: read body: {e}", server.host));
                    continue;
                }
            },
            Err(e) => {
                last_err = Some(format!("{}: {e}", server.host));
                continue;
            }
        };
        return parse_manifest(&raw, depot_key);
    }
    Err(DepotError::AllCdnHostsFailed(
        last_err.unwrap_or_else(|| "no error captured".into()),
    ))
}

/// Parse a downloaded manifest blob: unzip, walk the magic-prefixed sections,
/// turn the protobuf payload into our typed view, and decrypt filenames.
fn parse_manifest(raw: &[u8], depot_key: &DepotKey) -> Result<Manifest> {
    // CDN returns a ZIP with exactly one entry containing the section stream.
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(raw))?;
    if zip.len() != 1 {
        return Err(DepotError::ManifestMalformed);
    }
    let mut inner = Vec::new();
    zip.by_index(0)?.read_to_end(&mut inner)?;

    let (payload, metadata, _signature) = parse_sections(&inner)?;
    build_manifest(payload, metadata, depot_key)
}

fn parse_sections(
    inner: &[u8],
) -> Result<(
    ContentManifestPayload,
    ContentManifestMetadata,
    Option<ContentManifestSignature>,
)> {
    let mut payload = None;
    let mut metadata = None;
    let mut signature = None;
    let mut pos = 0usize;

    while pos + 4 <= inner.len() {
        let magic = u32::from_le_bytes(inner[pos..pos + 4].try_into().unwrap());
        pos += 4;
        if magic == MAGIC_END {
            break;
        }
        let len = u32::from_le_bytes(
            inner
                .get(pos..pos + 4)
                .ok_or(DepotError::ManifestMalformed)?
                .try_into()
                .unwrap(),
        ) as usize;
        pos += 4;
        let body = inner
            .get(pos..pos + len)
            .ok_or(DepotError::ManifestMalformed)?;
        pos += len;
        match magic {
            MAGIC_PAYLOAD => payload = Some(ContentManifestPayload::parse_from_bytes(body)?),
            MAGIC_METADATA => metadata = Some(ContentManifestMetadata::parse_from_bytes(body)?),
            MAGIC_SIGNATURE => signature = Some(ContentManifestSignature::parse_from_bytes(body)?),
            other => return Err(DepotError::UnknownManifestMagic(other)),
        }
    }

    Ok((
        payload.ok_or(DepotError::ManifestSectionMissing("payload"))?,
        metadata.ok_or(DepotError::ManifestSectionMissing("metadata"))?,
        signature,
    ))
}

fn build_manifest(
    payload: ContentManifestPayload,
    metadata: ContentManifestMetadata,
    depot_key: &DepotKey,
) -> Result<Manifest> {
    let needs_decrypt = metadata.filenames_encrypted();
    let mut files = Vec::with_capacity(payload.mappings.len());

    for m in payload.mappings {
        let raw_name = m.filename.unwrap_or_default();
        let path = if needs_decrypt {
            decrypt_filename(&raw_name, depot_key)?
        } else {
            raw_name.replace('\\', "/")
        };

        let flags = m.flags.unwrap_or(0);
        let kind = if flags & FLAG_DIRECTORY != 0 {
            DepotFileKind::Directory
        } else if flags & FLAG_SYMLINK != 0 {
            let target = m
                .linktarget
                .filter(|s| !s.is_empty())
                .ok_or(DepotError::ManifestMalformed)?;
            DepotFileKind::Symlink { target }
        } else {
            let sha: [u8; 20] = m
                .sha_content
                .ok_or(DepotError::ManifestMalformed)?
                .try_into()
                .map_err(|_| DepotError::ManifestMalformed)?;

            let mut chunks = m
                .chunks
                .into_iter()
                .map(|c| {
                    let sha: [u8; 20] = c
                        .sha
                        .ok_or(DepotError::ManifestMalformed)?
                        .try_into()
                        .map_err(|_| DepotError::ManifestMalformed)?;
                    Ok(Chunk {
                        sha: ChunkHash(sha),
                        crc: c.crc.unwrap_or(0),
                        offset: c.offset.unwrap_or(0),
                        size_uncompressed: c.cb_original.unwrap_or(0),
                        size_compressed: c.cb_compressed.unwrap_or(0),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            // Steam's wire format does not guarantee chunk ordering.
            chunks.sort_unstable_by_key(|c| c.offset);

            DepotFileKind::File {
                sha: FileHash(sha),
                executable: flags & FLAG_EXECUTABLE != 0,
                chunks,
            }
        };

        files.push(DepotFile {
            path,
            size: m.size.unwrap_or(0),
            kind,
        });
    }

    // Steam's wire format usually delivers files in path-sorted order, but
    // doesn't guarantee it. Sort so callers can rely on a deterministic
    // listing for diffs, pagination, etc.
    files.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(Manifest {
        depot_id: metadata.depot_id(),
        manifest_id: metadata.gid_manifest(),
        creation_time: metadata.creation_time(),
        size_uncompressed: metadata.cb_disk_original(),
        size_compressed: metadata.cb_disk_compressed(),
        files,
    })
}

/// Decrypt a base64-wrapped filename: first 16 bytes are an ECB-encrypted IV,
/// the rest is AES-256-CBC with PKCS7 padding. Path separators are normalised
/// to forward slashes.
fn decrypt_filename(b64: &str, depot_key: &DepotKey) -> Result<String> {
    // Steam wraps the base64 at 76 chars (MIME style); strip whitespace.
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let buf = BASE64.decode(&cleaned)?;
    if buf.len() < 32 || buf.len() % 16 != 0 {
        return Err(DepotError::FilenameCiphertextLength(buf.len()));
    }

    let key = depot_key.as_bytes();
    let cipher = Aes256::new(key.into());

    // First 16 bytes -> IV via ECB-decrypt.
    let mut iv: Array<u8, _> = Array::from(<[u8; 16]>::try_from(&buf[..16]).unwrap());
    cipher.decrypt_block(&mut iv);

    // Rest is CBC-decrypted with that IV.
    type Aes256CbcDec = cbc::Decryptor<Aes256>;
    let mut body = buf[16..].to_vec();
    let plain = Aes256CbcDec::new(key.into(), &iv)
        .decrypt_padded::<Pkcs7>(&mut body)
        .map_err(|_| DepotError::FilenameDecrypt)?;

    // Strip trailing NULs, normalise path separators.
    let trimmed = plain.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let s = String::from_utf8(plain[..trimmed].to_vec())
        .map_err(DepotError::FilenameUtf8)?
        .replace('\\', "/");
    Ok(s)
}
