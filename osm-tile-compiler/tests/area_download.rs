//! `compile --area`, end to end over HTTP: the built binary looks a region up
//! in a recorded Geofabrik index (`fixtures/geofabrik-index-v1-nogeom.json`),
//! downloads `fixtures/monaco.osm.pbf` from a local server in its place, and
//! compiles it. Failed downloads must leave nothing behind.
//!
//! `live_geofabrik_download` fetches Monaco from Geofabrik itself and checks
//! the recorded index against the live one. It needs the network, so it is
//! ignored by default; the nightly workflow runs it:
//!
//! ```sh
//! cargo test -p osm-tile-compiler --test area_download -- --ignored
//! ```

mod common;

use common::{monaco_pbf, synthetic_sources};
use osm_tile_compiler::{
    Extract, ExtractEvent,
    extracts::{GEOFABRIK_INDEX_URL, find_extract, parse_index},
    fetch_extract,
};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const INDEX_VAR: &str = "OSM_TILE_COMPILER_GEOFABRIK_INDEX";
const MONACO_PATH: &str = "/europe/monaco-latest.osm.pbf";

fn recorded_index() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/geofabrik-index-v1-nogeom.json");
    std::fs::read_to_string(path).unwrap()
}

/// How the local server answers a request for the extract.
#[derive(Clone, Copy)]
enum ExtractReply {
    /// The whole fixture.
    Whole,
    /// A `Content-Length` for the whole fixture, then half of it and a
    /// closed connection.
    CutShort,
    /// 404 Not Found.
    Missing,
}

/// Plays download.geofabrik.de on 127.0.0.1: serves the recorded index, with
/// its URLs pointed back at itself, and the Monaco fixture as Monaco's
/// extract. Every request's path is recorded.
struct Server {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(extract: ExtractReply) -> Self {
        Self::start_with(true, extract)
    }

    fn start_with(index_available: bool, extract: ExtractReply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let index = recorded_index().replace("https://download.geofabrik.de", &base);
        let pbf = std::fs::read(monaco_pbf()).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (index, pbf, recorded) = (index.clone(), pbf.clone(), Arc::clone(&recorded));
                std::thread::spawn(move || {
                    let _ = serve(stream, index_available, &index, extract, &pbf, &recorded);
                });
            }
        });
        Self { base, requests }
    }

    fn index_url(&self) -> String {
        format!("{}/index-v1-nogeom.json", self.base)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(
    stream: TcpStream,
    index_available: bool,
    index: &str,
    extract: ExtractReply,
    pbf: &[u8],
    requests: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" {
            break;
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    requests.lock().unwrap().push(path.clone());
    let mut stream = stream;
    let mut respond = |status: &str, content_type: &str, length: usize, body: &[u8]| {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
             Content-Length: {length}\r\nConnection: close\r\n\r\n"
        )?;
        stream.write_all(body)?;
        stream.flush()
    };
    match (path.as_str(), extract) {
        ("/index-v1-nogeom.json", _) if index_available => {
            respond("200 OK", "application/json", index.len(), index.as_bytes())
        }
        (MONACO_PATH, ExtractReply::Whole) => {
            respond("200 OK", "application/octet-stream", pbf.len(), pbf)
        }
        (MONACO_PATH, ExtractReply::CutShort) => respond(
            "200 OK",
            "application/octet-stream",
            pbf.len(),
            &pbf[..pbf.len() / 2],
        ),
        ("/index-v1-nogeom.json", _) => respond("503 Service Unavailable", "text/plain", 0, b""),
        _ => respond("404 Not Found", "text/plain", 9, b"not found"),
    }
}

/// Runs the binary with `index_url` as its Geofabrik index (the real one when
/// `None`). Against the local server, proxies are left out so that requests
/// to 127.0.0.1 go straight to it.
fn run(index_url: Option<&str>, args: &[&dyn AsRef<std::ffi::OsStr>]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_osm-tile-compiler"));
    command.args(args.iter().map(|arg| arg.as_ref()));
    if let Some(url) = index_url {
        command.env(INDEX_VAR, url);
        for proxy in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
            command.env_remove(proxy).env_remove(proxy.to_lowercase());
        }
    } else {
        command.env_remove(INDEX_VAR);
    }
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

struct Workspace {
    _temp: tempfile::TempDir,
    downloads: PathBuf,
    sources: PathBuf,
    root: PathBuf,
}

fn workspace() -> Workspace {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let sources = root.join("sources");
    synthetic_sources(&sources);
    Workspace {
        downloads: root.join("downloads"),
        sources,
        root,
        _temp: temp,
    }
}

/// `compile --area <name>` into `<root>/<output>`.
fn compile_area(index_url: Option<&str>, w: &Workspace, name: &str, output: &str) -> Output {
    run(
        index_url,
        &[
            &"compile",
            &"--area",
            &name,
            &"--download-dir",
            &w.downloads,
            &"--sources",
            &w.sources,
            &"--output",
            &w.root.join(output),
        ],
    )
}

fn download_dir_entries(w: &Workspace) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(&w.downloads) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn assert_verifies(archive: &Path) {
    let output = run(None, &[&"verify", &archive]);
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn downloads_compiles_and_then_reuses_the_extract_an_index_names() {
    let server = Server::start(ExtractReply::Whole);
    let index_url = server.index_url();
    let w = workspace();

    // By name, not identifier, so the lookup runs through the index.
    let output = compile_area(Some(&index_url), &w, "Monaco", "first.pmtiles");
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains(&format!("Looking up 'Monaco' in {index_url}")),
        "{stderr}"
    );
    assert!(
        stderr.contains("'Monaco' is Geofabrik's monaco (Monaco, in europe)"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "monaco.osm.pbf: downloading {}{MONACO_PATH}",
            server.base
        )),
        "{stderr}"
    );
    assert!(stderr.contains("monaco.osm.pbf: downloaded ("), "{stderr}");
    assert_eq!(server.requests(), ["/index-v1-nogeom.json", MONACO_PATH]);
    assert_eq!(download_dir_entries(&w), ["monaco.osm.pbf"]);
    assert_eq!(
        std::fs::read(w.downloads.join("monaco.osm.pbf")).unwrap(),
        std::fs::read(monaco_pbf()).unwrap(),
        "the extract on disk differs from what was served"
    );
    assert_verifies(&w.root.join("first.pmtiles"));

    // Again, by ISO code: the index is read, the extract is not downloaded.
    let output = compile_area(Some(&index_url), &w, "MC", "second.pmtiles");
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("already present, not downloaded again"),
        "{stderr}"
    );
    assert_eq!(
        server.requests(),
        [
            "/index-v1-nogeom.json",
            MONACO_PATH,
            "/index-v1-nogeom.json"
        ]
    );
    assert_eq!(
        std::fs::read(w.root.join("first.pmtiles")).unwrap(),
        std::fs::read(w.root.join("second.pmtiles")).unwrap(),
        "the same extract compiled to different archives"
    );
}

#[test]
fn a_download_cut_short_fails_and_leaves_no_file() {
    let server = Server::start(ExtractReply::CutShort);
    let w = workspace();
    let output = compile_area(Some(&server.index_url()), &w, "monaco", "out.pmtiles");
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    // ureq notices the short body before our length check does.
    assert!(
        stderr.contains(&format!(
            "Could not download {}{MONACO_PATH}: ",
            server.base
        )),
        "{stderr}"
    );
    assert_eq!(download_dir_entries(&w), Vec::<String>::new());
    assert!(!w.root.join("out.pmtiles").exists());
}

#[test]
fn a_missing_extract_fails_with_its_url_and_leaves_no_file() {
    let server = Server::start(ExtractReply::Missing);
    let w = workspace();
    let output = compile_area(Some(&server.index_url()), &w, "monaco", "out.pmtiles");
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!("Could not download {}{MONACO_PATH}", server.base)),
        "{stderr}"
    );
    assert!(stderr.contains("404"), "{stderr}");
    assert_eq!(download_dir_entries(&w), Vec::<String>::new());
}

#[test]
fn an_unknown_or_ambiguous_area_fails_before_any_download() {
    let server = Server::start(ExtractReply::Whole);
    let w = workspace();
    let output = compile_area(Some(&server.index_url()), &w, "Atlantis", "out.pmtiles");
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("Geofabrik has no extract named 'Atlantis'"),
        "{stderr}"
    );
    let output = compile_area(Some(&server.index_url()), &w, "Georgia ", "out.pmtiles");
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("georgia (Georgia), us/georgia (Georgia)"),
        "{stderr}"
    );
    assert_eq!(
        server.requests(),
        ["/index-v1-nogeom.json", "/index-v1-nogeom.json"]
    );
    assert!(!w.downloads.exists());
}

#[test]
fn an_unavailable_index_fails_with_its_url() {
    let server = Server::start_with(false, ExtractReply::Whole);
    let w = workspace();
    let output = compile_area(Some(&server.index_url()), &w, "monaco", "out.pmtiles");
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "Could not download Geofabrik's index from {}",
            server.index_url()
        )),
        "{stderr}"
    );
    assert_eq!(server.requests(), ["/index-v1-nogeom.json"]);
}

/// Monaco as the recorded index lists it.
fn recorded_monaco() -> Extract {
    let index = parse_index(recorded_index().as_bytes()).unwrap();
    find_extract(&index, "monaco").unwrap().clone()
}

#[test]
#[ignore = "downloads from download.geofabrik.de; run by the nightly workflow"]
fn live_geofabrik_download() {
    let w = workspace();

    // The binary, as a user runs it.
    let output = compile_area(None, &w, "monaco", "live.pmtiles");
    let stderr = text(&output.stderr);
    eprintln!("{stderr}");
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains(&format!("Looking up 'monaco' in {GEOFABRIK_INDEX_URL}")),
        "{stderr}"
    );
    assert!(stderr.contains("monaco.osm.pbf: downloaded ("), "{stderr}");
    assert_eq!(download_dir_entries(&w), ["monaco.osm.pbf"]);
    let bytes = std::fs::metadata(w.downloads.join("monaco.osm.pbf"))
        .unwrap()
        .len();
    assert!(bytes > 100_000, "Monaco's extract is only {bytes} bytes");
    assert_verifies(&w.root.join("live.pmtiles"));

    // The live index still lists Monaco as the recorded one does, so the
    // fixture tests above exercise what Geofabrik serves.
    let mut found = None;
    fetch_extract(
        &w.downloads,
        "monaco",
        |event| {
            if let ExtractEvent::Found { extract } = event {
                found = Some(extract.clone());
            }
        },
        || false,
    )
    .unwrap();
    assert_eq!(
        found.expect("no Found event"),
        recorded_monaco(),
        "Geofabrik's index has changed; update fixtures/geofabrik-index-v1-nogeom.json"
    );
}
