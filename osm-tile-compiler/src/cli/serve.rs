//! `serve`: archives and a style pack over HTTP, for a preview.
use super::{Command, number, option, value};
use crate::interrupt;
use osm_tile_compiler::{ServeRequest, TileServer};
use std::ffi::OsString;
use std::net::{IpAddr, UdpSocket};
use std::path::PathBuf;

#[derive(Debug)]
pub(super) struct ServeArgs {
    archives: Vec<PathBuf>,
    style_pack: Option<PathBuf>,
    host: String,
    port: u16,
}

pub(super) fn parse(args: &[OsString]) -> Result<Command, String> {
    let mut serve = ServeArgs {
        archives: Vec::new(),
        style_pack: None,
        host: "127.0.0.1".into(),
        port: 8080,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            serve.archives.push(PathBuf::from(arg));
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--style-pack" => serve.style_pack = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--host" => serve.host = value(&mut args, &flag)?.to_string_lossy().into_owned(),
            "--port" => serve.port = number(value(&mut args, &flag)?, &flag)?,
            _ => return Err(format!("unknown serve option '{flag}'")),
        }
    }
    if serve.archives.is_empty() {
        return Err("serve needs at least one archive".into());
    }
    Ok(Command::Serve(serve))
}

pub(super) fn run(args: ServeArgs) -> Result<(), String> {
    // An IPv6 literal needs brackets in an address.
    let address = if args.host.contains(':') && !args.host.starts_with('[') {
        format!("[{}]:{}", args.host, args.port)
    } else {
        format!("{}:{}", args.host, args.port)
    };
    let mut request = ServeRequest::new(args.archives, address);
    request.style_pack = args.style_pack;
    let server = TileServer::bind(&request).map_err(|e| e.to_string())?;
    let local = server.local_addr();
    let shown = if local.ip().is_unspecified() {
        format!("127.0.0.1:{}", local.port())
    } else {
        local.to_string()
    };
    for archive in server.archives() {
        let name = &archive.name;
        eprintln!(
            "Serving {} ({}, zooms {}-{}): style http://{shown}/styles/{name}.json, \
             map http://{shown}/maps/{name}",
            archive.path.display(),
            archive.format,
            archive.min_zoom,
            archive.max_zoom,
        );
    }
    match &request.style_pack {
        Some(pack) => eprintln!("Glyphs and sprite from {}", pack.display()),
        None => eprintln!("No --style-pack: the styles load glyphs and sprite from the network"),
    }
    if local.ip().is_unspecified()
        && let Some(lan) = lan_address()
    {
        eprintln!("Other devices: http://{lan}:{}/", local.port());
    }
    eprintln!("Listening on http://{shown}/; press Ctrl-C to stop");
    server.run(interrupt::requested).map_err(|e| e.to_string())
}

/// The address this machine reaches the network from, found by pointing a
/// UDP socket at a documentation address; nothing is sent.
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}
