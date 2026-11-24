//! Reads byte ranges of an archive served over HTTP, as PMTiles viewers do:
//! one `Range` request per span, against any server or object store that
//! answers them with `206 Partial Content`.
use std::time::Duration;

/// The first request reads this much: the header and, as the spec asks
/// writers to place it, the root directory.
const INITIAL_BYTES: u64 = 16_384;
/// Attempts per range before giving up on transient failures.
const ATTEMPTS: u32 = 4;

/// A file on an HTTP server, read by byte ranges.
pub(super) struct HttpFile {
    agent: ureq::Agent,
    pub(super) url: String,
    pub(super) length: u64,
    /// The validator of the first response; a later response with another
    /// means the file changed while it was read.
    etag: Option<String>,
    /// The bytes the first request returned, from the start of the file.
    initial: Vec<u8>,
    pub(super) requests: u64,
    pub(super) bytes_read: u64,
}

/// One response: its bytes, where they start, the file's length and its
/// ETag.
struct Part {
    bytes: Vec<u8>,
    start: u64,
    length: u64,
    etag: Option<String>,
}

/// The connect and response timeouts of the downloads, without their
/// two-hour cap on the whole transfer: one range never takes that long.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(Duration::from_secs(120)))
        .build()
        .into()
}

/// `bytes <first>-<last>/<length>`.
fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, length) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    Some((
        first.trim().parse().ok()?,
        last.trim().parse().ok()?,
        length.trim().parse().ok()?,
    ))
}

/// Whether a failure may pass when tried again: the network, a timeout, or
/// a server that is busy or failing.
fn transient(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::StatusCode(code) => *code == 408 || *code == 429 || *code >= 500,
        ureq::Error::Io(_)
        | ureq::Error::Timeout(_)
        | ureq::Error::ConnectionFailed
        | ureq::Error::HostNotFound => true,
        _ => false,
    }
}

/// GETs bytes `first..=last` of `url`. `whole_up_to` accepts a `200` with
/// the whole file when it is no longer than that, for servers that answer
/// a range covering a small file with all of it.
fn get(
    agent: &ureq::Agent,
    url: &str,
    first: u64,
    last: u64,
    whole_up_to: u64,
) -> Result<Part, String> {
    let mut delay = Duration::from_millis(500);
    let mut attempt = 1;
    loop {
        match get_once(agent, url, first, last, whole_up_to) {
            Ok(part) => return Ok(part),
            Err(Some(error)) if attempt < ATTEMPTS && transient(&error) => {
                std::thread::sleep(delay);
                delay *= 2;
                attempt += 1;
            }
            Err(Some(error)) => return Err(format!("Could not read {url}: {error}")),
            Err(None) => {
                return Err(format!(
                    "{url} does not answer range requests; extract reads remote \
                     archives by byte ranges, as PMTiles viewers do"
                ));
            }
        }
    }
}

/// `Err(None)` when the server ignores the range.
fn get_once(
    agent: &ureq::Agent,
    url: &str,
    first: u64,
    last: u64,
    whole_up_to: u64,
) -> Result<Part, Option<ureq::Error>> {
    let mut response = agent
        .get(url)
        .header("Range", format!("bytes={first}-{last}"))
        .call()
        .map_err(Some)?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let etag = header("etag");
    let status = response.status().as_u16();
    let (start, limit, length) = if status == 206 {
        let (start, end, length) = header("content-range")
            .as_deref()
            .and_then(content_range)
            .ok_or(None)?;
        if start != first || end < start {
            return Err(None);
        }
        (start, end - start + 1, Some(length))
    } else {
        // The whole file: only when it is small, and only from the start.
        let length = header("content-length").and_then(|v| v.parse::<u64>().ok());
        if first != 0 || length.is_none_or(|length| length > whole_up_to) {
            return Err(None);
        }
        (0, whole_up_to, None)
    };
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(Some)?;
    let length = length.unwrap_or(bytes.len() as u64);
    Ok(Part {
        bytes,
        start,
        length,
        etag,
    })
}

impl HttpFile {
    /// Reads the start of the archive at `url`.
    pub(super) fn open(url: &str) -> Result<Self, String> {
        let agent = agent();
        let part = get(&agent, url, 0, INITIAL_BYTES - 1, INITIAL_BYTES)?;
        debug_assert_eq!(part.start, 0);
        Ok(Self {
            agent,
            url: url.to_owned(),
            length: part.length,
            etag: part.etag,
            requests: 1,
            bytes_read: part.bytes.len() as u64,
            initial: part.bytes,
        })
    }

    /// Bytes `offset..offset + length`, which lie within the file.
    pub(super) fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let end = offset + length;
        if end <= self.initial.len() as u64 {
            return Ok(self.initial[offset as usize..end as usize].to_vec());
        }
        let mut parts = self.read_many(&[(offset, length)], 1)?;
        Ok(parts.pop().unwrap_or_default())
    }

    /// The bytes of each `(offset, length)`, which lie within the file, with
    /// up to `parallel` requests at a time.
    pub(super) fn read_many(
        &mut self,
        ranges: &[(u64, u64)],
        parallel: usize,
    ) -> Result<Vec<Vec<u8>>, String> {
        let fetch = |&(offset, length): &(u64, u64)| -> Result<Vec<u8>, String> {
            if length == 0 {
                return Ok(Vec::new());
            }
            let part = get(&self.agent, &self.url, offset, offset + length - 1, 0)?;
            if part.length != self.length
                || (part.etag.is_some() && self.etag.is_some() && part.etag != self.etag)
            {
                return Err(format!("{} changed while it was read", self.url));
            }
            if part.bytes.len() as u64 != length {
                return Err(format!(
                    "{} returned {} bytes for a range of {length}",
                    self.url,
                    part.bytes.len()
                ));
            }
            Ok(part.bytes)
        };
        let mut out = Vec::with_capacity(ranges.len());
        for group in ranges.chunks(parallel.max(1)) {
            let results: Vec<Result<Vec<u8>, String>> = if group.len() == 1 {
                vec![fetch(&group[0])]
            } else {
                std::thread::scope(|scope| {
                    let handles: Vec<_> = group
                        .iter()
                        .map(|range| scope.spawn(|| fetch(range)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|handle| {
                            handle
                                .join()
                                .unwrap_or_else(|_| Err("a range request panicked".into()))
                        })
                        .collect()
                })
            };
            for result in results {
                let bytes = result?;
                self.requests += 1;
                self.bytes_read += bytes.len() as u64;
                out.push(bytes);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_ranges_parse() {
        assert_eq!(
            content_range("bytes 0-16383/40960"),
            Some((0, 16_383, 40_960))
        );
        assert_eq!(content_range("bytes 10-20/*"), None);
        assert_eq!(content_range("items 0-1/2"), None);
    }
}
