//! Patches between two versions of a decoded tile, for delta archives: the
//! new tile as runs copied from the old one and bytes inserted between them,
//! deflated.
//!
//! Before deflating, all integers varints:
//!
//! ```text
//! new length
//! then, until that many bytes are written:
//!   insert length, then that many bytes
//!   copy length, and when it isn't 0, the copy's offset in the old tile as a
//!   zigzag difference from where the last copy ended
//! ```
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use rustc_hash::FxHashMap;
use std::io::{Read, Write};

/// The shortest run worth copying, and the width of the index's keys.
const WINDOW: usize = 8;

fn window(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + WINDOW].try_into().unwrap())
}

fn put(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn take(input: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *input.get(*at).ok_or("a tile patch ends early")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("a tile patch holds a varint longer than 10 bytes".into())
}

/// The patch that turns `old` into `new`.
pub(super) fn encode(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut index: FxHashMap<u64, u32> = FxHashMap::default();
    if old.len() >= WINDOW {
        index.reserve(old.len());
        for at in 0..=old.len() - WINDOW {
            index.entry(window(old, at)).or_insert(at as u32);
        }
    }
    let mut raw = Vec::new();
    put(&mut raw, new.len() as u64);
    let mut last_end = 0i64;
    let mut emit = |raw: &mut Vec<u8>, insert: &[u8], copy: Option<(usize, usize)>| {
        put(raw, insert.len() as u64);
        raw.extend_from_slice(insert);
        match copy {
            Some((offset, length)) => {
                put(raw, length as u64);
                let step = offset as i64 - last_end;
                put(raw, ((step << 1) ^ (step >> 63)) as u64);
                last_end = (offset + length) as i64;
            }
            None => put(raw, 0),
        }
    };
    let (mut at, mut pending) = (0usize, 0usize);
    while at + WINDOW <= new.len() {
        if let Some(&found) = index.get(&window(new, at)) {
            let found = found as usize;
            let forward = old[found..]
                .iter()
                .zip(&new[at..])
                .take_while(|(a, b)| a == b)
                .count();
            if forward >= WINDOW {
                let backward = old[..found]
                    .iter()
                    .rev()
                    .zip(new[pending..at].iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count();
                emit(
                    &mut raw,
                    &new[pending..at - backward],
                    Some((found - backward, forward + backward)),
                );
                at += forward;
                pending = at;
                continue;
            }
        }
        at += 1;
    }
    if pending < new.len() {
        emit(&mut raw, &new[pending..], None);
    }
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&raw).unwrap();
    encoder.finish().unwrap()
}

/// Applies `patch` to `old`; fails on a patch that would write more than
/// `limit` bytes or doesn't fit `old`.
pub(super) fn decode(old: &[u8], patch: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    DeflateDecoder::new(patch)
        .take(limit as u64 * 4 + 1024)
        .read_to_end(&mut raw)
        .map_err(|e| format!("a tile patch is not valid deflate: {e}"))?;
    let at = &mut 0;
    let length = take(&raw, at)? as usize;
    if length > limit {
        return Err("a tile patch is larger than its tile".into());
    }
    let mut new = Vec::with_capacity(length);
    let mut last_end = 0i64;
    while new.len() < length {
        let insert = take(&raw, at)? as usize;
        let bytes = raw
            .get(*at..*at + insert)
            .ok_or("a tile patch ends inside an insert")?;
        *at += insert;
        new.extend_from_slice(bytes);
        let copy = take(&raw, at)? as usize;
        if copy > 0 {
            let step = take(&raw, at)?;
            let step = (step >> 1) as i64 ^ -((step & 1) as i64);
            let offset = usize::try_from(last_end + step)
                .map_err(|_| "a tile patch copies from before the tile")?;
            let bytes = old
                .get(offset..offset + copy)
                .ok_or("a tile patch copies from beyond the old tile")?;
            new.extend_from_slice(bytes);
            last_end = (offset + copy) as i64;
        }
        if new.len() > length {
            return Err("a tile patch writes past its tile".into());
        }
    }
    if *at != raw.len() {
        return Err("a tile patch has bytes after its end".into());
    }
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let old: Vec<u8> = (0..20_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();
        let mut new = old.clone();
        new.splice(5_000..5_010, *b"edited!");
        new.extend_from_slice(b"appended at the end");
        new.drain(100..300);
        for (a, b) in [
            (&old[..], &new[..]),
            (&new[..], &old[..]),
            (&[][..], &new[..]),
            (&old[..], &[][..]),
            (&b"short"[..], &b"shorter"[..]),
        ] {
            let patch = encode(a, b);
            assert_eq!(decode(a, &patch, b.len()).unwrap(), b);
        }
        assert!(encode(&old, &new).len() < 200);
    }

    #[test]
    fn rejects_patches_that_do_not_fit() {
        let patch = encode(b"0123456789abcdef", b"0123456789abcdef!");
        assert!(decode(b"0123", &patch, 100).is_err());
        assert!(decode(b"0123456789abcdef", &patch, 3).is_err());
        assert!(decode(b"0123456789abcdef", b"not deflate", 100).is_err());
    }
}
