//! A small local HTTP server that serves archives to a browser or an app on
//! the same machine or the local network: their tiles, a TileJSON for each,
//! a MapLibre style for each, the glyphs and sprite of an offline style pack,
//! and a preview page. It is meant for previews and field use, with a few
//! clients, not as a production tile server.
//!
//! The URLs, relative to the server's address:
//!
//! ```text
//! /                                   the archives, with links
//! /index.json                         the same, as JSON
//! /tiles/<name>.json                  TileJSON 3.0.0 for an archive
//! /tiles/<name>/<z>/<x>/<y>.<ext>     a tile (204 when the archive has none)
//! /tiles/<name>.pmtiles               a PMTiles archive itself, with ranges
//! /styles/<name>.json                 the MapLibre style for an archive
//! /maps/<name>                        a page that shows the style
//! /fonts/<fontstack>/<range>.pbf      the style pack's glyphs
//! /sprite.json, /sprite.png, ...      the style pack's sprite
//! ```
//!
//! An archive's name is its file name without the extension. The styles
//! point at the server with absolute URLs, built from the `Host` the client
//! asked for, so a phone that loads `http://192.168.1.20:8080/styles/x.json`
//! gets URLs it can reach. Every response allows any origin (CORS).

use crate::CompileError;
use crate::archive::{ArchiveReader, Entry};
use pmtiles::{TileCoord, TileId};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Connections served at once; more are answered with 503.
const MAX_CONNECTIONS: usize = 128;
/// The request line and headers may not be longer than this.
const MAX_HEAD_BYTES: u64 = 16 * 1024;
/// How long a kept-alive connection may stay idle.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Leaf directories of one PMTiles archive kept in memory.
const LEAF_CACHE: usize = 256;
/// The MapLibre GL JS release the preview page loads.
const MAPLIBRE_GL: &str = "https://unpkg.com/maplibre-gl@6.14.0/dist";
/// The sprite files a style pack may hold.
const SPRITE_FILES: [&str; 4] = [
    "sprite.json",
    "sprite.png",
    "sprite@2x.json",
    "sprite@2x.png",
];

/// What [`TileServer::bind`] serves, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServeRequest {
    /// PMTiles or MBTiles archives (MBTiles when the extension is
    /// `.mbtiles`), each served under its file name without the extension.
    pub archives: Vec<PathBuf>,
    /// A style pack that `write_style_pack` wrote: its `style.json` is the
    /// base of every archive's style, and its glyphs and sprite are served.
    /// Without one, the styles load glyphs and sprite from the network.
    pub style_pack: Option<PathBuf>,
    /// The address to listen on, such as `127.0.0.1:8080`; `0.0.0.0:8080`
    /// for other devices on the network too. Port 0 picks a free port.
    pub address: String,
}

impl ServeRequest {
    pub fn new(archives: Vec<PathBuf>, address: impl Into<String>) -> Self {
        Self {
            archives,
            style_pack: None,
            address: address.into(),
        }
    }
}

/// An archive a [`TileServer`] serves.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ServedArchive {
    /// The name in its URLs.
    pub name: String,
    pub path: PathBuf,
    /// `pmtiles` or `mbtiles`.
    pub format: &'static str,
    pub min_zoom: u8,
    pub max_zoom: u8,
}

/// A bound server; [`TileServer::run`] serves until asked to stop.
pub struct TileServer {
    listener: TcpListener,
    state: Arc<State>,
}

struct State {
    archives: Vec<Archive>,
    style_pack: Option<StylePack>,
    local: SocketAddr,
}

struct StylePack {
    directory: PathBuf,
    style: Value,
}

struct Archive {
    info: ServedArchive,
    /// The TileJSON without `tiles`.
    tilejson: Value,
    /// `.mvt`, `.mlt`, `.png`, ...
    extension: &'static str,
    content_type: &'static str,
    /// The `Content-Encoding` of the stored tiles.
    encoding: Option<&'static str>,
    /// Whether the archive holds the `sar` layer.
    sar: bool,
    source: Source,
}

/// A tile's stored bytes and their `Content-Encoding`.
type StoredTile = (Vec<u8>, Option<&'static str>);

enum Source {
    // Boxed: the reader is far larger than the MBTiles connection.
    PmTiles(Box<Mutex<PmTiles>>),
    MbTiles(Mutex<rusqlite::Connection>),
}

struct PmTiles {
    reader: ArchiveReader,
    root: Arc<Vec<Entry>>,
    leaves: HashMap<u64, Arc<Vec<Entry>>>,
}

impl TileServer {
    /// Opens every archive and the style pack, and listens on the address.
    /// Fails with [`CompileError::InvalidRequest`] when an archive or the
    /// pack cannot be read, or two archives would share a name.
    pub fn bind(request: &ServeRequest) -> Result<Self, CompileError> {
        let invalid = CompileError::InvalidRequest;
        if request.archives.is_empty() {
            return Err(invalid("Give at least one archive to serve".into()));
        }
        let mut archives: Vec<Archive> = Vec::new();
        for path in &request.archives {
            let archive = Archive::open(path).map_err(invalid)?;
            if let Some(other) = archives.iter().find(|a| a.info.name == archive.info.name) {
                return Err(invalid(format!(
                    "{} and {} would both be served as '{}'; rename one",
                    other.info.path.display(),
                    path.display(),
                    archive.info.name
                )));
            }
            archives.push(archive);
        }
        let style_pack = request
            .style_pack
            .as_deref()
            .map(StylePack::open)
            .transpose()
            .map_err(invalid)?;
        let address = request
            .address
            .to_socket_addrs()
            .map_err(|e| invalid(format!("Invalid address '{}': {e}", request.address)))?
            .next()
            .ok_or_else(|| invalid(format!("Invalid address '{}'", request.address)))?;
        let listener = TcpListener::bind(address)
            .map_err(|e| CompileError::other(format!("Could not listen on {address}: {e}")))?;
        let local = listener
            .local_addr()
            .map_err(|e| CompileError::other(e.to_string()))?;
        Ok(Self {
            listener,
            state: Arc::new(State {
                archives,
                style_pack,
                local,
            }),
        })
    }

    /// The address it listens on, with the port picked for port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.state.local
    }

    pub fn archives(&self) -> Vec<ServedArchive> {
        self.state.archives.iter().map(|a| a.info.clone()).collect()
    }

    /// Serves until `should_stop` returns true, which is asked about every
    /// tenth of a second. Connections open then are finished in the
    /// background.
    ///
    /// Connections are accepted on a thread of their own that blocks in
    /// `accept`, so a new connection is taken at once rather than at the next
    /// poll; stopping wakes it with a connection to itself.
    pub fn run(self, should_stop: impl Fn() -> bool) -> Result<(), CompileError> {
        let stop = Arc::new(AtomicBool::new(false));
        let listener = self.listener;
        let state = self.state;
        let wake = wake_address(state.local);
        let accepting = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || accept_loop(&listener, &state, &stop))
        };
        while !should_stop() && !accepting.is_finished() {
            std::thread::sleep(Duration::from_millis(100));
        }
        stop.store(true, Ordering::SeqCst);
        // Unblock `accept`; if the connection fails the thread is stuck
        // until the next client, so don't wait for it then.
        if TcpStream::connect_timeout(&wake, Duration::from_secs(1)).is_ok() {
            let _ = accepting.join();
        }
        Ok(())
    }
}

/// What another server needs to know about an archive to serve it as
/// `serve` would: see [`crate::martin`].
pub(crate) struct ArchiveSummary {
    pub info: ServedArchive,
    /// The TileJSON without `tiles`: its `center` places a style's view.
    pub tilejson: Value,
    /// Whether the tiles are MapLibre Tiles.
    pub mlt: bool,
    /// The `Content-Encoding` of the stored tiles.
    pub encoding: Option<&'static str>,
    /// Whether the archive holds the `sar` layer.
    pub sar: bool,
}

/// Opens the archive at `path` as `serve` does and describes it.
pub(crate) fn summarize(path: &Path) -> Result<ArchiveSummary, String> {
    let archive = Archive::open(path)?;
    Ok(ArchiveSummary {
        mlt: archive.extension == "mlt",
        encoding: archive.encoding,
        sar: archive.sar,
        tilejson: archive.tilejson,
        info: archive.info,
    })
}

/// Where a connection reaches a listener bound to `local`: the loopback
/// address for an unspecified one.
fn wake_address(local: SocketAddr) -> SocketAddr {
    let mut wake = local;
    if wake.ip().is_unspecified() {
        wake.set_ip(match wake {
            SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
            SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    wake
}

fn accept_loop(listener: &TcpListener, state: &Arc<State>, stop: &AtomicBool) {
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let accepted = listener.accept();
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Running out of file descriptors and the like: wait and
                // go on rather than stop serving.
                eprintln!("serve: could not accept a connection: {e}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            let _ = refuse(stream);
            continue;
        }
        let state = Arc::clone(state);
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            let _ = serve_connection(&state, stream);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn refuse(mut stream: TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut response = Response::text(503, "Too many connections\n");
    response.close = true;
    response.write(&mut stream, false)
}

impl Archive {
    fn open(path: &Path) -> Result<Self, String> {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{} has no name to serve it under", path.display()))?
            .to_owned();
        let mbtiles = crate::OutputFormat::from_path(path) == crate::OutputFormat::MbTiles;
        if !path.is_file() {
            return Err(format!("The archive {} does not exist", path.display()));
        }
        if mbtiles {
            Self::open_mbtiles(path, name)
        } else {
            Self::open_pmtiles(path, name)
        }
    }

    fn open_pmtiles(path: &Path, name: String) -> Result<Self, String> {
        let mut reader = ArchiveReader::open(path)?;
        let header = reader.header.clone();
        let metadata = reader
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let metadata: Value = if metadata.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&metadata)
                .map_err(|e| format!("{}: the metadata is not JSON: {e}", path.display()))?
        };
        let root = reader
            .directory(header.root_offset, header.root_length)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let (extension, content_type) = tile_type(header.tile_type);
        let tilejson = tilejson(
            &name,
            &metadata,
            header.min_zoom,
            header.max_zoom,
            header.bounds,
            [
                header.center[0],
                header.center[1],
                f64::from(header.center_zoom),
            ],
        );
        Ok(Self {
            sar: has_sar(&tilejson),
            info: ServedArchive {
                name,
                path: path.to_owned(),
                format: "pmtiles",
                min_zoom: header.min_zoom,
                max_zoom: header.max_zoom,
            },
            tilejson,
            extension,
            content_type,
            encoding: match header.tile_compression {
                2 => Some("gzip"),
                3 => Some("br"),
                4 => Some("zstd"),
                _ => None,
            },
            source: Source::PmTiles(Box::new(Mutex::new(PmTiles {
                reader,
                root: Arc::new(root.into_iter().map(|(entry, _)| entry).collect()),
                leaves: HashMap::new(),
            }))),
        })
    }

    fn open_mbtiles(path: &Path, name: String) -> Result<Self, String> {
        use rusqlite::OpenFlags;
        let fail =
            |e: rusqlite::Error| format!("{} is not a readable MBTiles: {e}", path.display());
        let db = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(fail)?;
        let mut rows: HashMap<String, String> = HashMap::new();
        {
            let mut statement = db
                .prepare("SELECT name, value FROM metadata")
                .map_err(fail)?;
            let mut query = statement.query([]).map_err(fail)?;
            while let Some(row) = query.next().map_err(fail)? {
                let (Ok(key), Ok(value)) = (row.get::<_, String>(0), row.get::<_, String>(1))
                else {
                    continue;
                };
                rows.insert(key, value);
            }
        }
        // The tiles view must be there, or every tile would fail later.
        db.prepare("SELECT tile_data FROM tiles LIMIT 0")
            .map_err(fail)?;
        let numbers = |key: &str| -> Option<Vec<f64>> {
            let values: Vec<f64> = rows
                .get(key)?
                .split(',')
                .map(|n| n.trim().parse().ok())
                .collect::<Option<_>>()?;
            Some(values)
        };
        let zoom = |key: &str, default: u8| {
            rows.get(key)
                .and_then(|z| z.trim().parse().ok())
                .unwrap_or(default)
        };
        let (min_zoom, max_zoom) = (zoom("minzoom", 0), zoom("maxzoom", 14));
        let bounds = numbers("bounds")
            .and_then(|b| <[f64; 4]>::try_from(b).ok())
            .unwrap_or([-180.0, -85.051_129, 180.0, 85.051_129]);
        let center = numbers("center")
            .and_then(|c| <[f64; 3]>::try_from(c).ok())
            .unwrap_or([
                (bounds[0] + bounds[2]) / 2.0,
                (bounds[1] + bounds[3]) / 2.0,
                f64::from(min_zoom),
            ]);
        let mut metadata = rows
            .get("json")
            .and_then(|j| serde_json::from_str::<Value>(j).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        for key in ["name", "description", "attribution", "version"] {
            if let Some(value) = rows.get(key) {
                metadata[key] = Value::from(value.as_str());
            }
        }
        let (extension, content_type) = match rows.get("format").map(String::as_str) {
            Some("png") => tile_type(2),
            Some("jpg" | "jpeg") => tile_type(3),
            Some("webp") => tile_type(4),
            Some("mlt") => tile_type(6),
            _ => tile_type(1),
        };
        let tilejson = tilejson(&name, &metadata, min_zoom, max_zoom, bounds, center);
        Ok(Self {
            sar: has_sar(&tilejson),
            info: ServedArchive {
                name,
                path: path.to_owned(),
                format: "mbtiles",
                min_zoom,
                max_zoom,
            },
            tilejson,
            extension,
            content_type,
            // MBTiles stores vector tiles gzipped; each tile is checked.
            encoding: None,
            source: Source::MbTiles(Mutex::new(db)),
        })
    }

    /// The stored bytes of a tile and their encoding, or `None` when the
    /// archive has no such tile.
    fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<StoredTile>, String> {
        match &self.source {
            Source::PmTiles(pmtiles) => {
                let id = TileId::from(TileCoord::new(z, x, y).map_err(|e| e.to_string())?).value();
                let mut pmtiles = pmtiles.lock().unwrap_or_else(|p| p.into_inner());
                Ok(pmtiles.tile(id)?.map(|bytes| (bytes, self.encoding)))
            }
            Source::MbTiles(db) => {
                let db = db.lock().unwrap_or_else(|p| p.into_inner());
                let row = (1u32 << z) - 1 - y;
                let mut statement = db
                    .prepare_cached(
                        "SELECT tile_data FROM tiles \
                         WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
                    )
                    .map_err(|e| e.to_string())?;
                let mut rows = statement
                    .query(rusqlite::params![z, x, row])
                    .map_err(|e| e.to_string())?;
                let Some(row) = rows.next().map_err(|e| e.to_string())? else {
                    return Ok(None);
                };
                let bytes: Vec<u8> = row.get(0).map_err(|e| e.to_string())?;
                let encoding = bytes.starts_with(&[0x1f, 0x8b]).then_some("gzip");
                Ok(Some((bytes, encoding)))
            }
        }
    }
}

impl PmTiles {
    fn tile(&mut self, id: u64) -> Result<Option<Vec<u8>>, String> {
        let mut entries = Arc::clone(&self.root);
        // The root and up to three levels of leaves, as the reader allows.
        for _ in 0..4 {
            let at = entries.partition_point(|e| e.tile_id <= id);
            let Some(entry) = at.checked_sub(1).map(|i| entries[i]) else {
                return Ok(None);
            };
            if entry.run_length > 0 {
                return if id < entry.tile_id + u64::from(entry.run_length) {
                    self.reader.tile(&entry).map(Some)
                } else {
                    Ok(None)
                };
            }
            entries = self.leaf(entry)?;
        }
        Err("leaf directories nest too deep".into())
    }

    fn leaf(&mut self, pointer: Entry) -> Result<Arc<Vec<Entry>>, String> {
        if let Some(leaf) = self.leaves.get(&pointer.offset) {
            return Ok(Arc::clone(leaf));
        }
        let header = &self.reader.header;
        if pointer
            .offset
            .checked_add(u64::from(pointer.length))
            .is_none_or(|end| end > header.leaf_length)
        {
            return Err(format!(
                "leaf directory at {} lies beyond the leaf section",
                pointer.offset
            ));
        }
        let offset = header.leaf_offset + pointer.offset;
        let leaf: Arc<Vec<Entry>> = Arc::new(
            self.reader
                .directory(offset, u64::from(pointer.length))?
                .into_iter()
                .map(|(entry, _)| entry)
                .collect(),
        );
        if self.leaves.len() >= LEAF_CACHE {
            self.leaves.clear();
        }
        self.leaves.insert(pointer.offset, Arc::clone(&leaf));
        Ok(leaf)
    }
}

impl StylePack {
    fn open(directory: &Path) -> Result<Self, String> {
        let path = directory.join("style.json");
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "{} is not a style pack: could not read {}: {e}",
                directory.display(),
                path.display()
            )
        })?;
        let style: Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
        if !style["layers"].is_array() {
            return Err(format!("{} is not a MapLibre style", path.display()));
        }
        Ok(Self {
            directory: directory.to_owned(),
            style,
        })
    }
}

/// The extension in tile URLs and the `Content-Type` of a PMTiles tile type.
fn tile_type(code: u8) -> (&'static str, &'static str) {
    match code {
        2 => ("png", "image/png"),
        3 => ("jpg", "image/jpeg"),
        4 => ("webp", "image/webp"),
        5 => ("avif", "image/avif"),
        6 => ("mlt", "application/vnd.maplibre-vector-tile"),
        _ => ("mvt", "application/vnd.mapbox-vector-tile"),
    }
}

fn tilejson(
    name: &str,
    metadata: &Value,
    min_zoom: u8,
    max_zoom: u8,
    bounds: [f64; 4],
    center: [f64; 3],
) -> Value {
    let mut tilejson = json!({
        "tilejson": "3.0.0",
        "name": name,
        "scheme": "xyz",
        "minzoom": min_zoom,
        "maxzoom": max_zoom,
        "bounds": bounds,
        "center": center,
    });
    for key in [
        "name",
        "description",
        "attribution",
        "version",
        "vector_layers",
    ] {
        if let Some(value) = metadata.get(key) {
            tilejson[key] = value.clone();
        }
    }
    tilejson
}

fn has_sar(tilejson: &Value) -> bool {
    tilejson["vector_layers"]
        .as_array()
        .is_some_and(|layers| layers.iter().any(|l| l["id"] == crate::SAR_LAYER))
}

/// A request's method, path (percent-decoded segments) and the headers it
/// needs.
struct Request {
    head: bool,
    options: bool,
    segments: Vec<String>,
    host: Option<String>,
    gzip: bool,
    range: Option<String>,
    keep_alive: bool,
}

struct Response {
    status: u16,
    content_type: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Body,
    close: bool,
}

enum Body {
    Bytes(Vec<u8>),
    /// `length` bytes of a file from `offset`.
    File {
        file: File,
        offset: u64,
        length: u64,
    },
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            headers: Vec::new(),
            body: Body::Bytes(body),
            close: false,
        }
    }

    fn text(status: u16, text: &str) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            text.as_bytes().to_vec(),
        )
    }

    fn json(value: &Value) -> Self {
        let mut body = serde_json::to_vec_pretty(value).expect("JSON serializes");
        body.push(b'\n');
        Self::new(200, "application/json", body)
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn write(self, stream: &mut TcpStream, head_only: bool) -> io::Result<()> {
        let length = match &self.body {
            Body::Bytes(bytes) => bytes.len() as u64,
            Body::File { length, .. } => *length,
        };
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {length}\r\n\
             Access-Control-Allow-Origin: *\r\n",
            self.status,
            reason(self.status),
            self.content_type
        );
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if self.close {
            head.push_str("Connection: close\r\n");
        }
        head.push_str("\r\n");
        let mut out = io::BufWriter::with_capacity(64 * 1024, stream);
        out.write_all(head.as_bytes())?;
        if !head_only {
            match self.body {
                Body::Bytes(bytes) => out.write_all(&bytes)?,
                Body::File {
                    mut file,
                    offset,
                    length,
                } => {
                    file.seek(SeekFrom::Start(offset))?;
                    let copied = io::copy(&mut (&mut file).take(length), &mut out)?;
                    if copied != length {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the file got shorter",
                        ));
                    }
                }
            }
        }
        out.flush()
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn serve_connection(state: &State, mut stream: TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let request = match read_request(&mut reader) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(response) => {
                let mut response = *response;
                response.close = true;
                return response.write(&mut stream, false);
            }
        };
        let mut response = if request.options {
            Response::new(204, "text/plain", Vec::new())
                .header("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS")
                .header("Access-Control-Allow-Headers", "*")
                .header("Access-Control-Max-Age", "86400")
        } else {
            route(state, &request)
        };
        response.close |= !request.keep_alive;
        let close = response.close;
        response.write(&mut stream, request.head)?;
        if close {
            return Ok(());
        }
    }
}

/// Reads one request; `Ok(None)` when the client closed the connection or
/// went idle. Requests with a body are refused, as no URL takes one.
fn read_request(reader: &mut BufReader<TcpStream>) -> Result<Option<Request>, Box<Response>> {
    let mut budget = reader.take(MAX_HEAD_BYTES);
    let mut line = String::new();
    // Clients may send blank lines between requests.
    loop {
        line.clear();
        match budget.read_line(&mut line) {
            Ok(0) | Err(_) => return Ok(None),
            Ok(_) if line.trim().is_empty() => continue,
            Ok(_) => break,
        }
    }
    let too_large = || Box::new(Response::text(431, "Request headers too large\n"));
    let bad = |why: &str| Box::new(Response::text(400, &format!("{why}\n")));
    if !line.ends_with('\n') {
        return Err(too_large());
    }
    let mut parts = line.split_ascii_whitespace();
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("Malformed request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(bad("Only HTTP/1.x is served"));
    }
    let (method, target, http10) = (method.to_owned(), target.to_owned(), version == "HTTP/1.0");
    let mut host = None;
    let mut gzip = false;
    let mut range = None;
    let mut connection = String::new();
    loop {
        line.clear();
        match budget.read_line(&mut line) {
            Ok(0) | Err(_) => return Err(too_large()),
            Ok(_) => {}
        }
        if !line.ends_with('\n') {
            return Err(too_large());
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err(bad("Malformed header"));
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => host = Some(value.to_owned()),
            "accept-encoding" => gzip = value.to_ascii_lowercase().contains("gzip"),
            "range" => range = Some(value.to_owned()),
            "connection" => connection = value.to_ascii_lowercase(),
            "content-length" if value != "0" => return Err(bad("Requests have no body")),
            "transfer-encoding" => return Err(bad("Requests have no body")),
            _ => {}
        }
    }
    let keep_alive = if http10 {
        connection.contains("keep-alive")
    } else {
        !connection.contains("close")
    };
    let head = match method.as_str() {
        "GET" => false,
        "HEAD" => true,
        "OPTIONS" => {
            return Ok(Some(Request {
                head: false,
                options: true,
                segments: Vec::new(),
                host: None,
                gzip,
                range: None,
                keep_alive,
            }));
        }
        _ => {
            let mut response = Response::text(405, "Only GET, HEAD and OPTIONS are served\n")
                .header("Allow", "GET, HEAD, OPTIONS");
            response.close = !keep_alive;
            return Err(Box::new(response));
        }
    };
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let Some(path) = path.strip_prefix('/') else {
        return Err(bad("The path must start with /"));
    };
    let segments = path
        .split('/')
        .map(percent_decode)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| bad("Malformed percent-encoding in the path"))?;
    // `/` splits into one empty segment.
    let segments = if segments == [""] {
        Vec::new()
    } else {
        segments
    };
    Ok(Some(Request {
        head,
        options: false,
        segments,
        host,
        gzip,
        range,
        keep_alive,
    }))
}

fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = segment.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encodes a path segment: everything but unreserved characters.
fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn route(state: &State, request: &Request) -> Response {
    let segments: Vec<&str> = request.segments.iter().map(String::as_str).collect();
    let origin = origin(state, request.host.as_deref());
    let not_found = || Response::text(404, "Not found\n");
    let archive = |file: &str, extension: &str| {
        let name = file.strip_suffix(extension)?;
        state.archives.iter().find(|a| a.info.name == name)
    };
    match segments.as_slice() {
        [] => Response::new(
            200,
            "text/html; charset=utf-8",
            index_page(state).into_bytes(),
        )
        .header("Cache-Control", "no-cache"),
        ["index.json"] => Response::json(&Value::from(
            state
                .archives
                .iter()
                .map(|a| {
                    let name = percent_encode(&a.info.name);
                    let mut entry = serde_json::to_value(&a.info).expect("serializes");
                    entry["tilejson"] = format!("{origin}/tiles/{name}.json").into();
                    entry["style"] = format!("{origin}/styles/{name}.json").into();
                    entry["map"] = format!("{origin}/maps/{name}").into();
                    entry
                })
                .collect::<Vec<_>>(),
        ))
        .header("Cache-Control", "no-cache"),
        ["tiles", file] if file.ends_with(".json") => match archive(file, ".json") {
            Some(archive) => {
                let mut tilejson = archive.tilejson.clone();
                tilejson["tiles"] = json!([format!(
                    "{origin}/tiles/{}/{{z}}/{{x}}/{{y}}.{}",
                    percent_encode(&archive.info.name),
                    archive.extension
                )]);
                Response::json(&tilejson).header("Cache-Control", "no-cache")
            }
            None => not_found(),
        },
        ["tiles", file] if file.ends_with(".pmtiles") => match archive(file, ".pmtiles") {
            Some(archive) if archive.info.format == "pmtiles" => {
                raw_archive(&archive.info.path, request.range.as_deref())
            }
            _ => not_found(),
        },
        ["tiles", name, z, x, y] => {
            let Some(archive) = state.archives.iter().find(|a| a.info.name == *name) else {
                return not_found();
            };
            let y = y.split_once('.').map_or(*y, |(y, _)| y);
            let (Ok(z), Ok(x), Ok(y)) = (z.parse::<u8>(), x.parse::<u32>(), y.parse::<u32>())
            else {
                return not_found();
            };
            if z > 30 || x >= 1 << z || y >= 1 << z {
                return not_found();
            }
            tile(archive, z, x, y, request.gzip)
        }
        ["styles", file] => match archive(file, ".json") {
            Some(archive) => {
                Response::json(&style(state, archive, &origin)).header("Cache-Control", "no-cache")
            }
            None => not_found(),
        },
        ["maps", name] => match state.archives.iter().find(|a| a.info.name == *name) {
            Some(archive) => Response::new(
                200,
                "text/html; charset=utf-8",
                map_page(archive).into_bytes(),
            )
            .header("Cache-Control", "no-cache"),
            None => not_found(),
        },
        ["fonts", fontstack, range] => match &state.style_pack {
            Some(pack) => glyphs(pack, fontstack, range),
            None => not_found(),
        },
        [file] if SPRITE_FILES.contains(file) || *file == "LICENSE.md" => match &state.style_pack {
            Some(pack) => {
                let content_type = match file.rsplit_once('.').map(|(_, e)| e) {
                    Some("json") => "application/json",
                    Some("png") => "image/png",
                    _ => "text/markdown; charset=utf-8",
                };
                static_file(&pack.directory.join(file), content_type)
            }
            None => not_found(),
        },
        _ => not_found(),
    }
}

/// `http://<host>` for absolute URLs. The `Host` header is used when it
/// looks like a host and port, so clients get back the address they reached
/// the server at; otherwise the listening address.
fn origin(state: &State, host: Option<&str>) -> String {
    let valid = |host: &&str| {
        !host.is_empty()
            && host.len() <= 255
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.:[]_".contains(&b))
    };
    match host.filter(valid) {
        Some(host) => format!("http://{host}"),
        None => {
            let mut local = state.local;
            if local.ip().is_unspecified() {
                local.set_ip(if local.is_ipv4() {
                    std::net::Ipv4Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv6Addr::LOCALHOST.into()
                });
            }
            format!("http://{local}")
        }
    }
}

fn tile(archive: &Archive, z: u8, x: u32, y: u32, accepts_gzip: bool) -> Response {
    if z < archive.info.min_zoom || z > archive.info.max_zoom {
        return Response::new(204, archive.content_type, Vec::new());
    }
    match archive.tile(z, x, y) {
        Ok(Some((bytes, Some("gzip")))) if !accepts_gzip => {
            match crate::archive::decompress_gzip(&bytes) {
                Ok(bytes) => Response::new(200, archive.content_type, bytes),
                Err(e) => Response::text(500, &format!("{e}\n")),
            }
        }
        Ok(Some((bytes, encoding))) => {
            let response =
                Response::new(200, archive.content_type, bytes).header("Cache-Control", "no-cache");
            match encoding {
                Some(encoding) => response.header("Content-Encoding", encoding),
                None => response,
            }
        }
        Ok(None) => Response::new(204, archive.content_type, Vec::new()),
        Err(e) => {
            eprintln!("serve: {} {z}/{x}/{y}: {e}", archive.info.name);
            Response::text(500, &format!("{e}\n"))
        }
    }
}

/// The archive's style: the pack's, or the bundled one, with its source
/// pointing at the archive's TileJSON, glyphs and sprite at the pack's files
/// when there is a pack, the `sar` layers added when the archive has that
/// layer and the style lacks them, and the view starting at the archive's
/// center.
fn style(state: &State, archive: &Archive, origin: &str) -> Value {
    let mut style = match &state.style_pack {
        Some(pack) => pack.style.clone(),
        None => serde_json::from_str(&crate::maplibre_style("unused", None, None))
            .expect("the bundled style is JSON"),
    };
    let draws_sar = style["layers"]
        .as_array()
        .is_some_and(|layers| layers.iter().any(|l| l["source-layer"] == crate::SAR_LAYER));
    if archive.sar && !draws_sar {
        style = serde_json::from_str(&crate::with_sar_layers(&style.to_string()))
            .expect("the style is JSON");
    }
    let tilejson = format!("{origin}/tiles/{}.json", percent_encode(&archive.info.name));
    if let Some(sources) = style["sources"].as_object_mut() {
        for source in sources.values_mut() {
            if source["type"] == "vector" {
                source["url"] = tilejson.clone().into();
                // MapLibre decodes MapLibre Tiles only when told to.
                if archive.extension == "mlt" {
                    source["encoding"] = "mlt".into();
                }
                if let Some(source) = source.as_object_mut() {
                    source.remove("tiles");
                }
            }
        }
    }
    if state.style_pack.is_some() {
        style["glyphs"] = format!("{origin}/fonts/{{fontstack}}/{{range}}.pbf").into();
        style["sprite"] = format!("{origin}/sprite").into();
    }
    let center = &archive.tilejson["center"];
    if let (Some(lon), Some(lat), Some(zoom)) =
        (center[0].as_f64(), center[1].as_f64(), center[2].as_f64())
    {
        style["center"] = json!([lon, lat]);
        style["zoom"] = json!(zoom);
    }
    style
}

/// A glyph range of the pack. MapLibre asks for a comma-separated font stack
/// when a layer lists several fonts; the first one the pack has is served.
fn glyphs(pack: &StylePack, fontstack: &str, range: &str) -> Response {
    let not_found = || Response::text(404, "Not found\n");
    let Some(numbers) = range.strip_suffix(".pbf") else {
        return not_found();
    };
    if !numbers
        .split_once('-')
        .is_some_and(|(a, b)| a.parse::<u32>().is_ok() && b.parse::<u32>().is_ok())
    {
        return not_found();
    }
    for font in fontstack.split(',').map(str::trim) {
        if font.is_empty() || font.contains(['/', '\\', ':']) || font.starts_with('.') {
            continue;
        }
        let path = pack.directory.join("fonts").join(font).join(range);
        if path.is_file() {
            return static_file(&path, "application/x-protobuf");
        }
    }
    not_found()
}

fn static_file(path: &Path, content_type: &'static str) -> Response {
    match std::fs::read(path) {
        Ok(bytes) => {
            Response::new(200, content_type, bytes).header("Cache-Control", "max-age=3600")
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Response::text(404, "Not found\n"),
        Err(e) => Response::text(500, &format!("{e}\n")),
    }
}

/// The archive file, or the part of it one `Range: bytes=...` asks for, as
/// the PMTiles JavaScript library reads archives.
fn raw_archive(path: &Path, range: Option<&str>) -> Response {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) => return Response::text(500, &format!("{e}\n")),
    };
    let size = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(e) => return Response::text(500, &format!("{e}\n")),
    };
    let whole = |file| Response {
        status: 200,
        content_type: "application/octet-stream",
        headers: vec![("Accept-Ranges", "bytes".into())],
        body: Body::File {
            file,
            offset: 0,
            length: size,
        },
        close: false,
    };
    let Some(range) = range else {
        return whole(file);
    };
    let Some((start, end)) = parse_range(range, size) else {
        return Response::text(416, "Range not satisfiable\n")
            .header("Content-Range", format!("bytes */{size}"));
    };
    Response {
        status: 206,
        content_type: "application/octet-stream",
        headers: vec![
            ("Accept-Ranges", "bytes".into()),
            ("Content-Range", format!("bytes {start}-{end}/{size}")),
        ],
        body: Body::File {
            file,
            offset: start,
            length: end - start + 1,
        },
        close: false,
    }
}

/// One `bytes=a-b`, `bytes=a-` or `bytes=-n` range, as inclusive offsets
/// within `size`; `None` when it is not satisfiable or there are several.
fn parse_range(range: &str, size: u64) -> Option<(u64, u64)> {
    let spec = range.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let suffix: u64 = suffix.parse().ok()?;
            if suffix == 0 {
                return None;
            }
            (size.saturating_sub(suffix), size - 1)
        }
        (start, "") => (start.parse().ok()?, size - 1),
        (start, end) => (start.parse().ok()?, end.parse::<u64>().ok()?.min(size - 1)),
    };
    (start <= end && start < size).then_some((start, end))
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn index_page(state: &State) -> String {
    let mut rows = String::new();
    for archive in &state.archives {
        let name = percent_encode(&archive.info.name);
        rows.push_str(&format!(
            "<li><a href=\"maps/{name}\">{}</a> ({}, zooms {}-{}): \
             <a href=\"tiles/{name}.json\">TileJSON</a>, <a href=\"styles/{name}.json\">style</a></li>\n",
            html_escape(&archive.info.name),
            archive.info.format,
            archive.info.min_zoom,
            archive.info.max_zoom,
        ));
    }
    let pack = match &state.style_pack {
        Some(_) => "Glyphs and sprite come from the style pack.",
        None => "No style pack: the styles load glyphs and sprite from the network.",
    };
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>osm-tile-compiler serve</title>\n\
         <style>body {{ font: 15px/1.5 system-ui, sans-serif; margin: 2em; }}</style>\n\
         <h1>Archives</h1>\n<ul>\n{rows}</ul>\n<p>{pack} The map pages load MapLibre GL JS from \
         unpkg.com; apps can load the styles directly.</p>\n"
    )
}

fn map_page(archive: &Archive) -> String {
    let name = percent_encode(&archive.info.name);
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>{title}</title>\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <link rel=\"stylesheet\" href=\"{MAPLIBRE_GL}/maplibre-gl.css\">\n\
         <style>html, body, #map {{ margin: 0; height: 100%; }}</style>\n\
         <div id=\"map\"></div>\n\
         <script type=\"module\">\n\
         import {{ Map, NavigationControl }} from \"{MAPLIBRE_GL}/maplibre-gl.mjs\";\n\
         const map = new Map({{ container: \"map\", style: \"../styles/{name}.json\", hash: true }});\n\
         map.addControl(new NavigationControl());\n\
         window.map = map;\n\
         </script>\n",
        title = html_escape(&archive.info.name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{ArchiveSpec, MbtilesWriter, PmtilesArchiveWriter, TileWriter};
    use crate::{ArchiveCenter, CompileBounds};
    use std::sync::atomic::AtomicBool;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    /// Every tile of zoom 8 in a 160 x 160 block, enough for leaf
    /// directories, each holding its own coordinates.
    fn tiles() -> Vec<(u64, (u8, u32, u32))> {
        let mut tiles: Vec<_> = (0..160)
            .flat_map(|x| (0..160).map(move |y| (8u8, x, y)))
            .map(|(z, x, y)| {
                (
                    TileId::from(TileCoord::new(z, x, y).unwrap()).value(),
                    (z, x, y),
                )
            })
            .collect();
        tiles.sort_unstable();
        tiles
    }

    fn write_archives(dir: &Path) {
        let bounds = CompileBounds::new(60.0, 0.0, 45.0, -180.0);
        let metadata =
            r#"{"name":"Test","attribution":"OSM","vector_layers":[{"id":"water","fields":{}}]}"#;
        let spec = ArchiveSpec {
            bounds: &bounds,
            center: ArchiveCenter {
                longitude: 1.0,
                latitude: 2.0,
                zoom: 8,
            },
            metadata_json: metadata,
            min_zoom: 8,
            max_zoom: 8,
            tile_format: crate::TileFormat::Mvt,
            tile_compression: crate::TileCompression::Gzip,
        };
        let file = File::create(dir.join("test.pmtiles")).unwrap();
        let mut writers: Vec<Box<dyn TileWriter + '_>> = vec![
            Box::new(PmtilesArchiveWriter::create(&file, &spec).unwrap()),
            Box::new(MbtilesWriter::create(&dir.join("other.mbtiles"), &spec).unwrap()),
        ];
        for (id, (z, x, y)) in tiles() {
            let bytes = gzip(format!("{z}/{x}/{y}").as_bytes());
            for writer in &mut writers {
                writer.write_tile(id, &bytes).unwrap();
            }
        }
        for writer in writers {
            writer.finalize().unwrap();
        }
        let reader = ArchiveReader::open(&dir.join("test.pmtiles")).unwrap();
        assert!(
            reader.header.leaf_length > 0,
            "the lookup must follow leaves"
        );
        let pack = dir.join("pack");
        std::fs::create_dir_all(pack.join("fonts/Noto Sans Bold")).unwrap();
        std::fs::write(pack.join("fonts/Noto Sans Bold/0-255.pbf"), b"glyphs").unwrap();
        std::fs::write(pack.join("sprite.json"), b"{}").unwrap();
        std::fs::write(
            pack.join("style.json"),
            crate::maplibre_style("x", Some("fonts/{fontstack}/{range}.pbf"), Some("sprite")),
        )
        .unwrap();
    }

    /// Sends raw requests on one connection and returns each response's
    /// status line, headers (lowercased names) and body.
    fn exchange(
        address: SocketAddr,
        requests: &[&str],
    ) -> Vec<(String, HashMap<String, String>, Vec<u8>)> {
        let mut stream = TcpStream::connect(address).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut responses = Vec::new();
        for request in requests {
            stream.write_all(request.as_bytes()).unwrap();
            let mut status = String::new();
            reader.read_line(&mut status).unwrap();
            let mut headers = HashMap::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').unwrap();
                headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
            }
            let length: usize = headers["content-length"].parse().unwrap();
            let head = request.starts_with("HEAD");
            let mut body = vec![0; if head { 0 } else { length }];
            reader.read_exact(&mut body).unwrap();
            responses.push((status.trim_end().to_owned(), headers, body));
        }
        responses
    }

    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: maps.local:9000\r\nAccept-Encoding: gzip\r\n\r\n")
    }

    #[test]
    fn takes_each_new_connection_at_once_and_stops_promptly() {
        let dir = tempfile::tempdir().unwrap();
        write_archives(dir.path());
        let server = TileServer::bind(&ServeRequest::new(
            vec![dir.path().join("test.pmtiles")],
            "0.0.0.0:0",
        ))
        .unwrap();
        let address = wake_address(server.local_addr());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let running = std::thread::spawn(move || server.run(|| stopped.load(Ordering::Relaxed)));
        // One request per connection, one after another, as a client that
        // doesn't keep connections alive sends them. Polling for connections
        // every 100 ms took at least 5 s for these 50.
        let started = std::time::Instant::now();
        for _ in 0..50 {
            let response = exchange(address, &[&get("/tiles/test.json")]);
            assert!(response[0].0.starts_with("HTTP/1.1 200"));
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let asked = std::time::Instant::now();
        stop.store(true, Ordering::Relaxed);
        running.join().unwrap().unwrap();
        assert!(
            asked.elapsed() < Duration::from_secs(1),
            "{:?}",
            asked.elapsed()
        );
        assert!(TcpStream::connect(address).is_err(), "still listening");
    }

    #[test]
    fn serves_tiles_tilejson_styles_and_the_pack() {
        let dir = tempfile::tempdir().unwrap();
        write_archives(dir.path());
        let mut request = ServeRequest::new(
            vec![
                dir.path().join("test.pmtiles"),
                dir.path().join("other.mbtiles"),
            ],
            "127.0.0.1:0",
        );
        request.style_pack = Some(dir.path().join("pack"));
        let server = TileServer::bind(&request).unwrap();
        let address = server.local_addr();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let running = std::thread::spawn(move || server.run(|| stopped.load(Ordering::Relaxed)));

        // Every tile of both archives, on one kept-alive connection each.
        let all = tiles();
        for name in ["test", "other"] {
            let requests: Vec<String> = all
                .iter()
                .map(|(_, (z, x, y))| get(&format!("/tiles/{name}/{z}/{x}/{y}.mvt")))
                .collect();
            let requests: Vec<&str> = requests.iter().map(String::as_str).collect();
            let responses = exchange(address, &requests);
            for ((_, (z, x, y)), (status, headers, body)) in all.iter().zip(responses) {
                assert_eq!(status, "HTTP/1.1 200 OK", "{name} {z}/{x}/{y}");
                assert_eq!(headers["content-encoding"], "gzip");
                assert_eq!(headers["access-control-allow-origin"], "*");
                let text = crate::archive::decompress_gzip(&body).unwrap();
                assert_eq!(text, format!("{z}/{x}/{y}").as_bytes());
            }
        }

        let responses = exchange(
            address,
            &[
                &get("/tiles/test/8/200/3.mvt"),
                &get("/tiles/test/9/0/0.mvt"),
                &get("/tiles/test/8/256/0.mvt"),
                "GET /tiles/other/8/5/7.mvt HTTP/1.1\r\nHost: x\r\n\r\n",
                &get("/tiles/test.json"),
                &get("/styles/other.json"),
                &get("/fonts/Noto%20Sans%20Regular,Noto%20Sans%20Bold/0-255.pbf"),
                &get("/fonts/..%2Fsprite.json/0-255.pbf"),
                &get("/sprite.json"),
                &get("/tiles/other.pmtiles"),
                "GET /tiles/test.pmtiles HTTP/1.1\r\nHost: x\r\nRange: bytes=0-6\r\n\r\n",
                "HEAD /index.json HTTP/1.1\r\nHost: x\r\n\r\n",
                &get("/maps/test"),
                "OPTIONS /tiles/test.json HTTP/1.1\r\nHost: x\r\n\r\n",
            ],
        );
        let status = |i: usize| responses[i].0.split(' ').nth(1).unwrap().to_owned();
        assert_eq!(status(0), "204", "absent tile");
        assert_eq!(status(1), "204", "beyond the archive's zooms");
        assert_eq!(status(2), "404", "not a tile");
        assert_eq!(responses[3].2, b"8/5/7", "decompressed without gzip");
        assert!(!responses[3].1.contains_key("content-encoding"));
        let tilejson: Value = serde_json::from_slice(&responses[4].2).unwrap();
        assert_eq!(
            tilejson["tiles"][0],
            "http://maps.local:9000/tiles/test/{z}/{x}/{y}.mvt"
        );
        assert_eq!(tilejson["vector_layers"][0]["id"], "water");
        assert_eq!(tilejson["center"], json!([1.0, 2.0, 8.0]));
        let style: Value = serde_json::from_slice(&responses[5].2).unwrap();
        assert_eq!(
            style["sources"]["openmaptiles"]["url"],
            "http://maps.local:9000/tiles/other.json"
        );
        assert_eq!(
            style["glyphs"],
            "http://maps.local:9000/fonts/{fontstack}/{range}.pbf"
        );
        assert_eq!(style["sprite"], "http://maps.local:9000/sprite");
        assert_eq!(responses[6].2, b"glyphs", "the first font the pack has");
        assert_eq!(status(7), "404");
        assert_eq!(responses[8].2, b"{}");
        assert_eq!(status(9), "404", "MBTiles has no raw archive");
        assert_eq!(status(10), "206");
        assert_eq!(responses[10].2, b"PMTiles");
        assert_eq!(
            responses[10].1["content-range"].split('/').next(),
            Some("bytes 0-6")
        );
        assert_eq!(status(11), "200");
        assert!(responses[11].2.is_empty());
        assert!(String::from_utf8_lossy(&responses[12].2).contains("../styles/test.json"));
        assert_eq!(status(13), "204");

        stop.store(true, Ordering::Relaxed);
        running.join().unwrap().unwrap();
    }

    #[test]
    fn refuses_archives_sharing_a_name() {
        let dir = tempfile::tempdir().unwrap();
        write_archives(dir.path());
        std::fs::create_dir(dir.path().join("b")).unwrap();
        std::fs::copy(
            dir.path().join("test.pmtiles"),
            dir.path().join("b/test.pmtiles"),
        )
        .unwrap();
        let request = ServeRequest::new(
            vec![
                dir.path().join("test.pmtiles"),
                dir.path().join("b/test.pmtiles"),
            ],
            "127.0.0.1:0",
        );
        let Err(CompileError::InvalidRequest(message)) = TileServer::bind(&request) else {
            panic!("two archives named test");
        };
        assert!(message.contains("both be served as 'test'"), "{message}");
    }

    #[test]
    fn refuses_bodies_and_other_methods() {
        let dir = tempfile::tempdir().unwrap();
        write_archives(dir.path());
        let server = TileServer::bind(&ServeRequest::new(
            vec![dir.path().join("test.pmtiles")],
            "127.0.0.1:0",
        ))
        .unwrap();
        let address = server.local_addr();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let running = std::thread::spawn(move || server.run(|| stopped.load(Ordering::Relaxed)));
        let post = exchange(
            address,
            &["POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n"],
        );
        assert!(post[0].0.starts_with("HTTP/1.1 405"));
        let body = exchange(
            address,
            &["GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\n\r\nabcd"],
        );
        assert!(body[0].0.starts_with("HTTP/1.1 400"));
        assert_eq!(body[0].1["connection"], "close");
        stop.store(true, Ordering::Relaxed);
        running.join().unwrap().unwrap();
    }

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
    }

    #[test]
    fn percent_codes_round_trip() {
        assert_eq!(percent_encode("Noto Sans Bold"), "Noto%20Sans%20Bold");
        assert_eq!(
            percent_decode("Noto%20Sans%20Bold").as_deref(),
            Some("Noto Sans Bold")
        );
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%2"), None);
    }
}
