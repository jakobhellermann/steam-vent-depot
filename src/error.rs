use steam_vent::{EResult, NetworkError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DepotError {
    #[error("steam network error: {0}")]
    Network(#[from] NetworkError),

    #[error("steam server returned {0:?}")]
    Steam(EResult),

    #[error("app {0} not found")]
    AppNotFound(u32),

    #[error("app {app} has no depot {depot}")]
    DepotNotFound { app: u32, depot: u32 },

    #[error("app {app} depot {depot} has no branch {branch:?}")]
    BranchNotFound {
        app: u32,
        depot: u32,
        branch: String,
    },

    #[error("Steam response malformed: {0}")]
    ResponseMalformed(&'static str),

    #[error("invalid manifest gid {0:?}")]
    InvalidManifestGid(String),

    #[error("VDF parse: {0}")]
    Vdf(#[from] vdf_reader::error::VdfError),

    #[error("VDF was not valid UTF-8: {0}")]
    VdfUtf8(#[from] std::str::Utf8Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("no CDN hosts available")]
    NoCdnHosts,

    #[error("all CDN hosts failed; last error: {0}")]
    AllCdnHostsFailed(String),

    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("chunk zip container has {0} entries, expected 1")]
    ChunkZipEntryCount(usize),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("manifest section truncated or invalid")]
    ManifestMalformed,

    #[error("unknown manifest section magic 0x{0:08X}")]
    UnknownManifestMagic(u32),

    #[error("manifest missing {0} section")]
    ManifestSectionMissing(&'static str),

    #[error("protobuf parse: {0}")]
    Protobuf(#[from] steam_vent_proto_common::protobuf::Error),

    #[error("filename base64 decode: {0}")]
    FilenameBase64(#[from] base64::DecodeError),

    #[error("filename ciphertext length {0} is invalid (must be multiple of 16, ≥ 32)")]
    FilenameCiphertextLength(usize),

    #[error("filename CBC decrypt failed")]
    FilenameDecrypt,

    #[error("filename was not valid UTF-8 after decrypt: {0}")]
    FilenameUtf8(std::string::FromUtf8Error),

    #[error("depot key must be 32 bytes, got {0}")]
    DepotKeyLength(usize),

    #[error("file {0:?} not found in manifest")]
    FileNotFound(String),

    #[error("chunk container has bad length {0}")]
    ChunkCiphertextLength(usize),

    #[error("chunk AES-CBC decrypt failed")]
    ChunkDecrypt,

    #[error("unsupported chunk compression: first 4 bytes {0:02x?}")]
    UnsupportedCompression([u8; 4]),

    #[error("VZip container malformed: {0}")]
    VZipMalformed(&'static str),

    #[error("LZMA decompress: {0}")]
    Lzma(#[from] lzma_rs::error::Error),

    #[error("chunk Adler-32 mismatch: expected {expected:#010x}, got {got:#010x}")]
    ChunkCrcMismatch { expected: u32, got: u32 },

    #[error("chunk decompressed size mismatch: expected {expected}, got {got}")]
    ChunkSizeMismatch { expected: u32, got: u32 },
}

pub type Result<T, E = DepotError> = std::result::Result<T, E>;
