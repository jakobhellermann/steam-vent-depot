//! Chunk download + decrypt + decompress.
//!
//! Each chunk on the CDN is AES-256-CBC encrypted (same first-16-bytes-via-ECB
//! IV trick as filenames), and the plaintext inside is a Valve-flavoured
//! compression container. We support `VZa` (LZMA) today; `VSZa` (Zstd) and
//! `PK\x03\x04` (PKzip) come back as a clear [`DepotError::UnsupportedCompression`].

use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

use aes::Aes256;
use aes::cipher::{
    Array, BlockCipherDecrypt, BlockModeDecrypt, KeyInit, KeyIvInit, block_padding::Pkcs7,
};

use crate::cdn::CdnServer;
use crate::error::{DepotError, Result};
use crate::manifest::{Chunk, DepotKey};

/// Per-process round-robin index over the CDN host list. Each chunk
/// request starts at a different host so concurrent fetches spread
/// across the pool instead of all hammering `cdn_servers[0]`. On
/// failure we continue through the list as before, so this preserves
/// the "first 2xx wins" failover behavior.
static CDN_RR: AtomicUsize = AtomicUsize::new(0);

/// Download a chunk from a CDN host, decrypt, decompress, and verify against
/// the manifest's expected Adler-32 + decompressed size.
///
/// Hosts are tried starting at a round-robin offset (so parallel callers
/// don't pile onto the same host); subsequent retries advance through
/// the list. The first 2xx wins.
pub(crate) async fn fetch_chunk(
    http: &reqwest::Client,
    cdn_servers: &[CdnServer],
    depot_id: u32,
    chunk: &Chunk,
    depot_key: &DepotKey,
) -> Result<Vec<u8>> {
    if cdn_servers.is_empty() {
        return Err(DepotError::NoCdnHosts);
    }

    use tracing::Instrument as _;
    let start = CDN_RR.fetch_add(1, Ordering::Relaxed);
    let mut last_err: Option<String> = None;
    for i in 0..cdn_servers.len() {
        let server = &cdn_servers[start.wrapping_add(i) % cdn_servers.len()];
        let sha_hex = chunk.sha.to_string();
        let url = format!(
            "{base}/depot/{depot_id}/chunk/{sha_hex}",
            base = server.base_url()
        );
        let http_span = tracing::info_span!(
            "cdn.http_get",
            host = %server.host,
            size_compressed = chunk.size_compressed,
        );
        let send_res = async { http.get(&url).send().await }
            .instrument(http_span)
            .await;
        let resp = match send_res {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(format!("{}: {e}", server.host));
                continue;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            // Steam's CDN does not send `Retry-After` on 503s (verified
            // empirically across cache*.steamcontent.com and the
            // Akamai/Fastly/Google-fronted hosts), so there's no
            // useful header to surface here.
            last_err = Some(format!("{}: HTTP {status}", server.host));
            continue;
        }
        match resp
            .bytes()
            .instrument(tracing::info_span!("cdn.http_body"))
            .await
        {
            Ok(b) => {
                // AES-CBC decrypt + LZMA decompress + Adler-32 add up
                // to ~5–20 ms of CPU per chunk; offload so concurrent
                // fetches actually parallelise across worker threads.
                let chunk_clone = chunk.clone();
                let depot_key = depot_key.clone();
                let parent = tracing::Span::current();
                return tokio::task::spawn_blocking(move || {
                    let _g = tracing::info_span!(
                        parent: &parent,
                        "chunk.decode",
                        size_uncompressed = chunk_clone.size_uncompressed,
                    )
                    .entered();
                    decode_chunk(&b, &chunk_clone, &depot_key)
                })
                .await
                .map_err(|e| std::io::Error::other(format!("decode task panicked: {e}")))?;
            }
            Err(e) => last_err = Some(format!("{}: read body: {e}", server.host)),
        }
    }
    Err(DepotError::AllCdnHostsFailed(
        last_err.unwrap_or_else(|| "no error captured".into()),
    ))
}

/// Decrypt + decompress + verify a single chunk's bytes (as returned by the CDN).
fn decode_chunk(raw: &[u8], chunk: &Chunk, depot_key: &DepotKey) -> Result<Vec<u8>> {
    let plain = decrypt(raw, depot_key)?;
    let decompressed = decompress(&plain)?;

    if decompressed.len() as u32 != chunk.size_uncompressed {
        return Err(DepotError::ChunkSizeMismatch {
            expected: chunk.size_uncompressed,
            got: decompressed.len() as u32,
        });
    }
    let got = steam_adler32(&decompressed);
    if got != chunk.crc {
        return Err(DepotError::ChunkCrcMismatch {
            expected: chunk.crc,
            got,
        });
    }
    Ok(decompressed)
}

fn decrypt(raw: &[u8], depot_key: &DepotKey) -> Result<Vec<u8>> {
    if raw.len() < 32 || !(raw.len() - 16).is_multiple_of(16) {
        return Err(DepotError::ChunkCiphertextLength(raw.len()));
    }
    let key = depot_key.as_bytes();
    let cipher = Aes256::new(key.into());

    // First 16 bytes -> IV via ECB-decrypt.
    let mut iv: Array<u8, _> = Array::from(<[u8; 16]>::try_from(&raw[..16]).unwrap());
    cipher.decrypt_block(&mut iv);

    // Rest is CBC-decrypted with that IV.
    type Aes256CbcDec = cbc::Decryptor<Aes256>;
    let mut body = raw[16..].to_vec();
    let plain = Aes256CbcDec::new(key.into(), &iv)
        .decrypt_padded::<Pkcs7>(&mut body)
        .map_err(|_| DepotError::ChunkDecrypt)?;
    Ok(plain.to_vec())
}

fn decompress(plain: &[u8]) -> Result<Vec<u8>> {
    if plain.len() < 4 {
        return Err(DepotError::VZipMalformed("buffer shorter than magic"));
    }
    let magic: [u8; 4] = plain[..4].try_into().unwrap();
    if &magic == b"VSZa" {
        return decompress_vzstd(plain);
    }
    // VZip header is `V Z a <version-byte>`; the 4th byte is the version
    // ('a' today, anything else is unknown).
    if &magic[..3] == b"VZa" {
        return decompress_vzip(plain);
    }
    Err(DepotError::UnsupportedCompression(magic))
}

/// VZip container: `VZ` + version `a` + 4 byte timestamp + 5 byte LZMA props
/// + body + 4 byte CRC32 + 4 byte decompressed size + footer (`zv`).
fn decompress_vzip(buf: &[u8]) -> Result<Vec<u8>> {
    // Header: 2 byte magic (VZ) + 1 byte version (a) + 4 byte timestamp = 7 bytes
    // Then 5 byte LZMA properties, then the LZMA body, then footer.
    // Footer: 4 byte CRC32 + 4 byte decompressed size + 2 byte magic (zv) = 10 bytes
    const HEADER_LEN: usize = 2 + 1 + 4;
    const LZMA_PROPS_LEN: usize = 5;
    const FOOTER_LEN: usize = 4 + 4 + 2;

    if buf.len() < HEADER_LEN + LZMA_PROPS_LEN + FOOTER_LEN {
        return Err(DepotError::VZipMalformed("buffer too small"));
    }
    if &buf[0..3] != b"VZa" {
        return Err(DepotError::VZipMalformed("bad header magic"));
    }
    let footer = &buf[buf.len() - FOOTER_LEN..];
    if &footer[8..10] != b"zv" {
        return Err(DepotError::VZipMalformed("bad footer magic"));
    }
    let decompressed_size = u32::from_le_bytes(footer[4..8].try_into().unwrap()) as usize;
    // LZMA properties start at HEADER_LEN and are 5 bytes wide. lzma-rs's
    // raw_decoder takes a 5-byte properties header followed by the compressed
    // stream; we don't have a length-prefix LZMA "alone" framing, so use the
    // raw API.

    let props = &buf[HEADER_LEN..HEADER_LEN + LZMA_PROPS_LEN];
    let body = &buf[HEADER_LEN + LZMA_PROPS_LEN..buf.len() - FOOTER_LEN];

    // lzma-rs expects "lzma alone" style framing (5 props + 8 byte uncompressed length
    // + compressed bitstream). Reconstruct that since the VZ format omits the length.
    let mut framed = Vec::with_capacity(LZMA_PROPS_LEN + 8 + body.len());
    framed.extend_from_slice(props);
    framed.extend_from_slice(&(decompressed_size as u64).to_le_bytes());
    framed.extend_from_slice(body);

    let mut out = Vec::with_capacity(decompressed_size);
    lzma_rs::lzma_decompress(&mut Cursor::new(framed), &mut out)?;
    Ok(out)
}

/// VZstd container: `VSZa` (4) + crc32 (4) + zstd body + footer.
/// Footer is 15 bytes: crc32 (4) + decompressed_size (4) + unknown (4) + `zsv` (3).
fn decompress_vzstd(buf: &[u8]) -> Result<Vec<u8>> {
    const HEADER_LEN: usize = 4 + 4; // magic + leading crc32
    const FOOTER_LEN: usize = 4 + 4 + 4 + 3;

    if buf.len() < HEADER_LEN + FOOTER_LEN {
        return Err(DepotError::VZipMalformed("vzstd buffer too small"));
    }
    if &buf[0..4] != b"VSZa" {
        return Err(DepotError::VZipMalformed("bad vzstd header magic"));
    }
    let footer = &buf[buf.len() - FOOTER_LEN..];
    if &footer[FOOTER_LEN - 3..] != b"zsv" {
        return Err(DepotError::VZipMalformed("bad vzstd footer magic"));
    }
    let decompressed_size = u32::from_le_bytes(footer[4..8].try_into().unwrap()) as usize;

    let body = &buf[HEADER_LEN..buf.len() - FOOTER_LEN];
    let out = zstd::bulk::decompress(body, decompressed_size)
        .map_err(|_| DepotError::VZipMalformed("zstd decompress failed"))?;
    Ok(out)
}

/// Adler-32 starting from seed 0 (Steam's variant), not the conventional seed 1.
/// Matches SteamKit's `Adler32.Calculate( 0, input )`.
fn steam_adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut s1: u32 = 0;
    let mut s2: u32 = 0;
    for &b in data {
        s1 = (s1 + b as u32) % MOD;
        s2 = (s2 + s1) % MOD;
    }
    (s2 << 16) | s1
}
