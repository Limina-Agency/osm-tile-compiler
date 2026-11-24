//! `search-index` and `search`: writing an archive's search index, and
//! querying it.
use super::{Command, Status, megabytes, number, option, parse_bounds, value};
use crate::interrupt;
use osm_tile_compiler::{
    CompileBounds, CompileError, SearchHit, SearchIndex, SearchIndexReport, SearchQuery,
    search_index_path, write_search_index_cancellable,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(super) struct SearchIndexArgs {
    input: PathBuf,
    output: Option<PathBuf>,
    threads: Option<usize>,
    json: bool,
}

#[derive(Debug)]
pub(super) struct SearchArgs {
    index: PathBuf,
    text: String,
    near: Option<[f64; 2]>,
    within: Option<CompileBounds>,
    layers: Vec<String>,
    limit: usize,
    json: bool,
}

pub(super) fn parse_search_index(args: &[OsString]) -> Result<Command, String> {
    let mut input = None;
    let mut output = None;
    let mut threads = None;
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            if input.is_some() {
                return Err(format!(
                    "unexpected argument '{}': search-index takes one archive",
                    arg.to_string_lossy()
                ));
            }
            input = Some(PathBuf::from(arg));
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--output" | "-o" => output = Some(PathBuf::from(value(&mut args, "--output")?)),
            "--threads" => {
                let count: usize = number(value(&mut args, &flag)?, &flag)?;
                if count == 0 {
                    return Err("--threads must be at least 1".into());
                }
                threads = Some(count);
            }
            "--json" => json = true,
            _ => return Err(format!("unknown search-index option '{flag}'")),
        }
    }
    Ok(Command::SearchIndex(SearchIndexArgs {
        input: input.ok_or("search-index needs an archive")?,
        output,
        threads,
        json,
    }))
}

pub(super) fn parse_search(args: &[OsString]) -> Result<Command, String> {
    let mut index = None;
    let mut words = Vec::new();
    let mut near = None;
    let mut within = None;
    let mut layers = Vec::new();
    let mut limit = 10;
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            if index.is_none() {
                index = Some(PathBuf::from(arg));
            } else {
                words.push(arg.to_string_lossy().into_owned());
            }
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--near" => {
                let mut values = [0.0_f64; 2];
                for slot in &mut values {
                    let arg = args.next().ok_or("--near needs two numbers: LON LAT")?;
                    *slot = number(arg, &flag)?;
                }
                let [lon, lat] = values;
                if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
                    return Err(format!(
                        "--near must be LON LAT in degrees, got {lon} {lat}"
                    ));
                }
                near = Some(values);
            }
            "--within" => within = Some(parse_bounds(&mut args, "--within")?),
            "--layers" => {
                layers = value(&mut args, &flag)?
                    .to_string_lossy()
                    .split(',')
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            }
            "--limit" => {
                limit = number(value(&mut args, &flag)?, &flag)?;
                if limit == 0 {
                    return Err("--limit must be at least 1".into());
                }
            }
            "--json" => json = true,
            _ => return Err(format!("unknown search option '{flag}'")),
        }
    }
    let index = index.ok_or("search needs an index or archive and a query")?;
    if words.is_empty() {
        return Err("search needs a query after the index".into());
    }
    Ok(Command::Search(SearchArgs {
        index,
        text: words.join(" "),
        near,
        within,
        layers,
        limit,
        json,
    }))
}

pub(super) fn search_index(args: SearchIndexArgs) -> Result<(), String> {
    let mut status = Status::new();
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let output = args
        .output
        .unwrap_or_else(|| search_index_path(&args.input));
    status.line(&format!(
        "Writing the search index of {} into {} ({threads} threads)",
        args.input.display(),
        output.display()
    ));
    let _cooperating = interrupt::cooperate();
    let report =
        write_search_index_cancellable(&args.input, &output, threads, interrupt::requested)
            .map_err(|error| match error {
                CompileError::Cancelled => {
                    format!("cancelled; {} was not written", output.display())
                }
                error => error.to_string(),
            })?;
    report_index(&mut status, &output, &report);
    if args.json {
        let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        println!("{json}");
    }
    Ok(())
}

pub(super) fn report_index(status: &mut Status, path: &Path, report: &SearchIndexReport) {
    let layers: Vec<String> = report
        .layers
        .iter()
        .map(|(layer, count)| format!("{layer} {count}"))
        .collect();
    status.line(&format!(
        "Wrote {}: {} names, {} words, {} in {:.1} s ({})",
        path.display(),
        report.records,
        report.words,
        megabytes(report.bytes),
        report.seconds,
        layers.join(", ")
    ));
}

pub(super) fn search(args: SearchArgs) -> Result<(), String> {
    // An archive stands for the index beside it.
    let path = if args.index.extension().is_some_and(|e| e == "pmtiles") {
        search_index_path(&args.index)
    } else {
        args.index.clone()
    };
    if path != args.index && !path.exists() {
        return Err(format!(
            "{} does not exist; write it with 'search-index {}'",
            path.display(),
            args.index.display()
        ));
    }
    let index = SearchIndex::open(&path)?;
    let mut query = SearchQuery::new(args.text);
    query.near = args.near;
    query.within = args.within;
    query.layers = args.layers;
    query.limit = args.limit;
    let hits = index.search(&query)?;
    if args.json {
        let json = serde_json::to_string_pretty(&hits).map_err(|e| e.to_string())?;
        println!("{json}");
        return Ok(());
    }
    if hits.is_empty() {
        eprintln!("Nothing found for '{}'", query.text);
    }
    for hit in &hits {
        println!("{}", line(hit));
    }
    Ok(())
}

/// `Długa, Kraków  (transportation_name minor)  19.94321 50.06912  1.2 km`.
fn line(hit: &SearchHit) -> String {
    let mut text = hit.name.clone();
    if let Some(context) = &hit.context {
        text.push_str(", ");
        text.push_str(context);
    }
    if let Some(reference) = hit.reference.as_ref().filter(|r| **r != hit.name) {
        text.push_str(&format!(" [{reference}]"));
    }
    let mut kind = format!("{} {}", hit.layer, hit.class);
    if !hit.subclass.is_empty() && hit.subclass != hit.class {
        kind.push('/');
        kind.push_str(&hit.subclass);
    }
    let mut line = format!("{text}  ({kind})  {:.5} {:.5}", hit.longitude, hit.latitude);
    if let Some(ele) = hit.ele {
        line.push_str(&format!("  ele {ele} m"));
    }
    if let Some(d) = hit.distance_m {
        line.push_str(&if d < 1000.0 {
            format!("  {d:.0} m away")
        } else {
            format!("  {:.1} km away", d / 1000.0)
        });
    }
    line
}
