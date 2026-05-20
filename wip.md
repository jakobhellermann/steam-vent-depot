# steam-vent-depot

Rust library für Steam-Depot-Manifests + Chunks, gebaut auf [`steam-vent`](https://codeberg.org/steam-vent/steam-vent).
Macht ungefähr das was DepotDownloader (C#/SteamKit2) tut, aber als Library statt CLI.

## Status

Funktioniert. Komplette Pipeline läuft: Login → AppInfo → DepotKey → CDN → ManifestRequestCode →
Manifest-Download/Decrypt/Parse → Chunk-Download/Decrypt/Decompress/Verify → File-Reassembly.

Getestet end-to-end mit Hollow Knight Silksong (app 1030300, depot 1030303 = Linux). Sowohl
LZMA-chunks (`VZa`-Container) als auch Zstd-chunks (`VSZa`-Container) gehen durch.

Nicht (noch) implementiert: PKzip-Container (`PK\x03\x04`), parallele Chunk-Downloads, Caching,
Web-API-Zugriff (alles über CM-Channel + CDN-HTTPS).

## Layout

```
src/
  lib.rs       DepotClient mit allen Methoden
  app_info.rs  AppInfo (typed view über die PICS-VDF response)
  cdn.rs       CdnServer discovery + URL-building
  manifest.rs  Manifest download + parse + filename-decrypt
  chunk.rs    Chunk download + decrypt + decompress + Adler-32 verify
  error.rs     DepotError (thiserror)

examples/
  list_depots.rs    PICS-flow, druckt depot/branch/manifest-gid für eine app
  cdn.rs            listet CDN-Hosts sortiert nach load
  fetch_manifest.rs komplette pipeline bis Manifest-View, druckt files
  fetch_file.rs     fetch_manifest + zusätzlich Chunks runter und schreibt eine Datei
```

Login-Helper ist in jedem Example als `mod login { ... }` dupliziert. Refactor nach `examples/common/` ist offen.

## User-facing API (in `pub use`)

```rust
DepotClient::{new, with_http, connection, app_info, cdn_servers, cdn_servers_for_cell,
              depot_key, manifest_request_code, fetch_manifest, fetch_chunk}
AppInfo { app_id, depots: DepotInfos }
DepotInfos { depots: HashMap<u32, Depot>, branches: HashMap<String, Branch>, private_branches: bool }
Depot { config, manifests: HashMap<String, ManifestRef>, system_defined, depot_from_app, shared_install }
ManifestRef { gid, size, download }     // pointer aus app_info
Branch { build_id, time_updated, time_build_updated, description }
CdnServer { kind, host, vhost, https, cell_id, load, weighted_load, allowed_apps }
  .base_url(), .allows_app(app_id), .manifest_url(...)
CdnKind { Cdn, SteamCache, OpenCache, Other(String) }
HttpsSupport { Mandatory, Optional, None, Unknown }
DepotKey([u8; 32])
Manifest { depot_id, manifest_id, creation_time, size_uncompressed, size_compressed, files: Vec<DepotFile> }
  .find_file(path)
DepotFile { path, size, kind: FileKind, sha, linktarget, chunks: Vec<Chunk> }
FileKind { File, Directory, Symlink }
Chunk { sha: [u8;20], crc: u32, offset: u64, size_uncompressed, size_compressed }
DepotError, Result<T, E = DepotError>
```

PICS/Pics ist intern. User sieht `app_info` / `AppInfo`. PICS wird einmal im doc-comment von
`app_info.rs` erwähnt als "fyi if you're debugging".

## Wie die Pipeline zusammenhängt

Sieben Schritte, ergibt die `fetch_file.rs` flow:

1. **PICS app_info** (CM-channel) — `CMsgClientPICSProductInfoRequest`. Buffer ist UTF-8 VDF mit
   einem `appinfo`-key am root. Wir filtern auf `depots.<id>.manifests.<branch>.gid/size`.
2. **Manifest gid** — aus dem AppInfo-Tree, kein extra Roundtrip.
3. **Depot key** (CM) — `CMsgClientGetDepotDecryptionKey`. 32 bytes AES-256, ändert sich nie.
4. **Manifest request code** (CM service method) — `ContentServerDirectory.GetManifestRequestCode`.
   Einmal-Token, kurzlebig.
5. **CDN servers** (CM service method) — `ContentServerDirectory.GetServersForSteamPipe`.
   Wir filtern auf https-capable, sortieren by `weighted_load` asc.
6. **Manifest download** (HTTPS, kein CM) — GET `https://<cdn>/depot/<depot>/manifest/<gid>/5/<request_code>`.
   `5` ist die `MANIFEST_VERSION` Konstante aus SteamKit.
7. **Manifest decrypt + parse** — entpacke ZIP (1 entry), walk magic-prefixed sections
   (`0x71F617D0` payload, `0x1F4812BE` metadata, `0x1B81B817` signature, `0x32C415AB` end),
   decrypte filenames mit depot_key falls `metadata.filenames_encrypted`.

Pro Chunk dann:
8. **Chunk download** (HTTPS) — GET `https://<cdn>/depot/<depot>/chunk/<sha1-hex>`. Keine auth nötig.
9. **Chunk decrypt** — AES-256-CBC. IV ist die ersten 16 bytes per AES-ECB-decrypt entschlüsselt
   (selber trick wie bei filenames).
10. **Chunk decompress** — magic-byte dispatch:
    - `VZa<…>` (3 byte magic) → VZip-container → LZMA
    - `VSZa` (4 byte magic) → VZstd-container → Zstd
    - `PK\x03\x04` → PKzip → noch nicht implementiert
11. **Chunk verify** — Adler-32 (Steam-Variante mit seed 0, **nicht** Standard-seed 1!) gegen `chunk.crc`,
    plus size-check.
12. **File assembly** — chunks nach `offset` sortieren, write_at, fertig.

## Gotchas / nicht-offensichtliche Sachen

### Auth
- `Connection::access(refresh_token)` reuses login ohne Steam-Guard-Prompt. Token ~200 Tage gültig.
  Heißt im Code `access_token`, ist aber der **refresh token** (ja, naming ist verwirrend, siehe
  Kommentar in `steam-vent/src/connection/unauthenticated.rs:153`).
- `FileGuardDataStore` (machine token) wird nur bei Email-Code- oder TOTP-Login befüllt. Mobile-App-
  Bestätigung produziert kein `new_guard_data`. Deshalb ist refresh-token-caching für echte Accounts
  essenziell. (Steam-vent baut das nicht built-in ein — wir machen's manuell im example.)
- `with_cell(4)` (London) im example weil Frankfurt-CMs zeitweise 502 zurückgaben. Cell-IDs in
  `CellMap.vdf` auf SteamDatabase/SteamTracking GitHub.

### VDF / app info schema
- Top-level wrapping in einem `"appinfo"` key — wir deserialisieren in `AppInfoEnvelope` und nehmen `.appinfo`.
- `depots.*` ist eine schmutzige map: numerische depot-IDs mischen sich mit special keys
  (`branches`, `privatebranches`, `workshopdepot`, `overridescddb`, `baselanguages`, …). Wir machen
  custom `Deserialize` für `DepotInfos` der das routet. Unknown special-keys werden silent skipped.
- VDF kennt nur strings. `vdf-reader` 0.3.3 hat einen `deserialize_u64` der von string parst —
  also `pub size: u64` deserialisiert sauber ohne custom helper.
- Booleans: Steam schreibt `"0"` oder `"1"`. `de_vdf_bool` ist strict — alles andere → error
  (damit wir's merken wenn Valve was ändert).
- Buffer ist NUL-terminiert. `trim_end_matches('\0')` ist nötig vorm Parser.
- Es gibt **kein offizielles Schema**. SteamDB cached PICS-data informell, ist die de-facto Quelle
  fürs Erkunden ungewohnter Felder.

### Manifest binary format
- ZIP-container mit genau 1 entry, drin ein magic-prefixed section stream.
- Filenames sind base64 (MIME-style mit Zeilenumbrüchen — Whitespace strippen vor decode!).
- File-flags (`FLAG_DIRECTORY = 0x40`, `FLAG_SYMLINK = 0x200`) sind nicht im proto-File als enum.
  Werte aus SteamKit übernommen.
- `MAGIC_END = 0x32C415AB` markiert das Ende; bricht die parsing-Loop ab.
- Mein `Chunk.sha` ist SHA-1 (20 byte), `chunk.crc` ist Adler-32 (4 byte). Vorher hatte ich das
  durcheinander, ist korrigiert.

### Chunk container quirks
- **VZip** layout: `VZ`(2) + `a`(1) + timestamp(4) + LZMA-props(5) + body + crc32(4) + size(4) + `zv`(2).
  Header total 7 bytes vor den LZMA props (nicht 8 wie ich erst dachte).
- LZMA-body ist nicht "alone" framing — die 8-byte decompressed-length fehlt. Ich rekonstruiere die
  framing für `lzma-rs::lzma_decompress`.
- **VZstd** layout: `VSZa`(4) + crc32(4) + zstd-body + footer(15).
  Footer: crc32(4) + size(4) + 4 unknown bytes + `zsv`(3). Die mysteriösen 4 footer bytes ignoriert
  auch SteamKit.
- **Steam Adler-32** startet mit seed `0`, nicht `1`. Standard-Implementierungen (`adler` crate) sind
  daher inkompatibel. Hand-rolled 8-Zeilen-Variante in `chunk.rs::steam_adler32`.

### URL format
```
https://<vhost-or-host>/depot/<depot>/manifest/<gid>/5/<request_code>
https://<vhost-or-host>/depot/<depot>/chunk/<sha1-hex>
```
Die `5` ist `MANIFEST_VERSION` aus SteamKit `CDN/Client.cs`. Manifest braucht request_code, chunks
nicht (chunks sind content-addressed → keine extra auth, integrity über SHA-1).

## Dependencies

| Crate | Zweck | Warum diese |
|---|---|---|
| `steam-vent` 0.5 | CM-channel + auth | parent crate |
| `steam-vent-proto` 0.5 | protobuf message types | von steam-vent verwendet |
| `steam-vent-proto-common` 0.5 | RpcMethod trait für service-method-calls | von proto verwendet |
| `reqwest` 0.13 | HTTPS für CDN | matches steam-vent's reqwest |
| `vdf-reader` 0.3 | VDF parser mit serde-deserializer | einziges aktiv gepflegtes VDF-crate |
| `serde` 1 | derive für AppInfo | Standard |
| `aes` 0.9, `cbc` 0.2 | AES-256-CBC für filenames + chunks | API changed vs 0.8: `BlockCipherDecrypt` + `BlockModeDecrypt` traits, `Array` statt `GenericArray`, `decrypt_padded` statt `decrypt_padded_mut` |
| `zip` 8.6 | ZIP für manifest container | aktuelle major; `ZipArchive::new` + `by_index` API unverändert vs 2.x |
| `base64` 0.22 | filename decoding | Standard |
| `lzma-rs` 0.3 | LZMA für VZip chunks | pure rust, keine system-deps |
| `zstd` 0.13 | Zstd für VZstd chunks | bindings, aber default-features=false |
| `thiserror` 2 | DepotError | Standard |

## Wo's Performance-Bottlenecks gibt

`fetch_file` für die 10 MB Datei: ~3.3s end-to-end. Aufschlüsselung (gemessen mit instrumented run):

- Login (cached refresh-token): ~1.0s — `ServerList::discover()` (~350ms HTTPS) + WS-connect + ClientLogon
- app_info + depot_key + manifest_request_code: ~300-500ms (drei CM-roundtrips)
- cdn_servers: ~250ms
- Manifest download + decrypt + parse: ~600ms (für ~800 KB ZIP)
- 12 chunk downloads + decompress: **sequenziell**, ~80-150ms pro chunk → ~1.2s

Parallel chunks würden wahrscheinlich auf ~500ms downdrücken. Stream-statt-Buffer für die output-Datei
auch — aktuell allokieren wir den ganzen file in memory.

Cached ServerList könnte die ersten 350ms killen.

## Was als nächstes anstehen würde

In ungefährer Prio-Reihenfolge:
1. **README.md** — wir haben aktuell nur diese CLAUDE.md. Public-facing README mit quick-start.
2. **Login-helper extrahieren** in `examples/common/mod.rs`, weil's in 4 Examples dupliziert ist.
3. **Parallel chunk downloads** mit `futures::stream::buffer_unordered`.
4. **`testing.rs`** aus dem steam-vent repo aufräumen — der ganze manifest-code dort ist obsolet.
5. **Tests**: integration-test der gegen einen kleinen Public-App läuft (kein login nötig — anonymes
   login klappt für PICS von public apps).
6. **PKzip container** support, falls jemals ein Depot das nutzt.
7. **Chunk caching** auf disk (chunks sind content-addressed via SHA-1, dedup ist gratis).
8. **`Manifest::write_file(&depot_client, &cdn_servers, depot_id, depot_key, file, output)`** als
   higher-level helper — was aktuell in `fetch_file.rs` als example-code steht, gehört in die Library.
9. **Bytes type für sizes** — aktuell sind sizes nackte `u64`. Eine `Bytes` newtype mit Display
   würde human-bytes nicht in jedem example dupliziert werden.

## Hinweise für zukünftige Sessions

- **Login-Token in `~/.cache/steam-vent/refresh_tokens.json`**. Live tokens — nicht aus dem
  Terminal-Output kopieren-pasten oder commiten. Wenn versehentlich exposed: in Steam → Authorized
  Devices revoken.
- **Test-Account**: `n8nam4test` (Email-Auth, keine Mobile-App). Echter Account: `jjakobh`.
- **`run_secret.sh`** im steam-vent-Verzeichnis hat das Pass hardcoded — bitte nicht ins repo
  comitten und beim ersten Schritt `--release` weglassen (dev-build reicht).
- **PICS für public apps geht auch anonym** via `Connection::anonymous(server_list)` — für Tests die
  keine Real-Credentials wollen.
- **NetHook**: zum tracen kann man depotdownloader-git mit der `DEPOTDOWNLOADER_NETHOOK=1` env var
  bauen (siehe `~/dev/contrib/DepotDownloader/DepotDownloader/Steam3Session.cs`, da ist ein patch
  drin). Schreibt jedes Protobuf einzeln nach `./nethook/<unixtime>/`.
- **NetHookAnalyzer2** ist das Tool um die .bin files zu inspizieren (WinForms, läuft aber unter mono).
- **SteamKit2 source** ist die einzige verlässliche Reference für die Wire-Details (Manifest binary
  format, chunk container, magic numbers, …). Valve hat nichts veröffentlicht.
- **SteamDB** (steamdb.info) ist gut zum Cross-Checken von PICS-Werten (manifest-IDs, branches, sizes).
- **steamcontent.com Frankfurt** (cell 5/87) hatte 502-Outages — ein Run gegen fra2 timeouted, einer
  klappt. Cell 4 (London) ist stabil als fallback.
- **`steam-client-rs` auf crates.io ist sketchy** — repo-link 404, halluzinierte Referenzen auf
  "nickalverson/node-steam-user" (echt ist DoctorMcKay), Vorsicht.

## Wenn was kaputt geht

- **`502 Bad Gateway`** auf WebSocket-connect: CM-Server-Pool-Problem, anderen Cell probieren.
- **`expected one of item, quoted item, ...`** beim VDF-parse: Buffer-NUL-Terminierung nicht
  gestrippt.
- **`unsupported chunk compression: [56, 53, 5a, 61]`**: `VSZa` = Zstd, sollte unterstützt sein.
  Wenn anderer magic: SteamKit `DepotChunk.Process()` checken.
- **`chunk Adler-32 mismatch`**: prüfe ob du Standard-Adler verwendest (seed=1) statt Steam-Variante
  (seed=0).
- **`AppInfoMalformed("unknown EResult on GetDepotDecryptionKey")`**: Account hat keinen Zugriff auf
  das Depot. Account muss die App owned haben.
