//! Runs `osm-tile-compiler serve`: its usage errors, and a real server on a
//! free port answering over HTTP until it is stopped.

use pmtiles::{Compression, PmTilesWriter, TileCoord, TileType};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_osm-tile-compiler");

#[test]
fn usage_errors_exit_with_2() {
    for (args, expected) in [
        (&["serve"][..], "serve needs at least one archive"),
        (
            &["serve", "a.pmtiles", "--frob"],
            "unknown serve option '--frob'",
        ),
        (
            &["serve", "a.pmtiles", "--port", "x"],
            "--port expects a number",
        ),
        (
            &["serve", "a.pmtiles", "--style-pack"],
            "--style-pack needs a value",
        ),
    ] {
        let output = Command::new(BIN).args(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
}

#[test]
fn a_missing_archive_fails() {
    let output = Command::new(BIN)
        .args(["serve", "missing.pmtiles", "--port", "0"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("does not exist"), "{stderr}");
}

#[test]
fn serves_an_archive_until_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("krakow.pmtiles");
    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .precompressed(Compression::Gzip)
        .min_zoom(14)
        .max_zoom(14)
        .bounds(19.8, 49.9, 20.1, 50.2)
        .center(19.9, 50.0)
        .metadata(r#"{"name":"Kraków","vector_layers":[{"id":"water","fields":{}}]}"#)
        .create(File::create(&archive).unwrap())
        .unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(b"tile").unwrap();
    let tile = gzip.finish().unwrap();
    writer
        .add_tile(TileCoord::new(14, 9100, 5550).unwrap(), &tile)
        .unwrap();
    writer.finalize().unwrap();

    let mut server = Command::new(BIN)
        .arg("serve")
        .arg(&archive)
        .args(["--port", "0"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(server.stderr.take().unwrap());
    let mut printed = String::new();
    let address = loop {
        let mut line = String::new();
        assert!(stderr.read_line(&mut line).unwrap() > 0, "{printed}");
        printed.push_str(&line);
        if let Some(rest) = line.strip_prefix("Listening on http://") {
            break rest.split('/').next().unwrap().to_owned();
        }
    };
    assert!(printed.contains("/styles/krakow.json"), "{printed}");

    let get = |path: &str| {
        let mut stream = TcpStream::connect(&address).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&response[..split]).into_owned();
        (head, response[split + 4..].to_vec())
    };
    let (head, body) = get("/tiles/krakow/14/9100/5550.mvt");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, tile);
    let (head, _) = get("/tiles/krakow/14/9101/5550.mvt");
    assert!(head.starts_with("HTTP/1.1 204"), "{head}");
    let (_, body) = get("/styles/krakow.json");
    let style: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        style["sources"]["openmaptiles"]["url"],
        format!("http://{address}/tiles/krakow.json")
    );
    let (_, body) = get("/tiles/krakow.json");
    let tilejson: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(tilejson["name"], "Kraków");
    assert_eq!(tilejson["minzoom"], 14);

    server.kill().unwrap();
    server.wait().unwrap();
}
