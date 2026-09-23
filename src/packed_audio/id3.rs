//! The small ID3 subset needed by RFC 8216 packed audio. Other frames are
//! skipped by their declared size; no metadata/codec dependency is needed.

use oxideav_core::{Error, ReadSeek, Result};

const MAX_METADATA_BYTES: usize = 1024 * 1024;
const OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp";

fn invalid() -> Error {
    Error::invalid("hls: malformed packed audio ID3 tag")
}

fn synchsafe(bytes: &[u8]) -> Result<usize> {
    if bytes.len() != 4 || bytes.iter().any(|b| b & 0x80 != 0) {
        return Err(invalid());
    }
    Ok(bytes.iter().fold(0, |n, b| (n << 7) | *b as usize))
}

fn be_size(bytes: &[u8]) -> Result<usize> {
    Ok(u32::from_be_bytes(bytes.try_into().map_err(|_| invalid())?) as usize)
}

fn deunsync(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        out.push(bytes[i]);
        if bytes[i] == 0xff && bytes.get(i + 1) == Some(&0) {
            i += 1;
        }
        i += 1;
    }
    out
}

/// `ID3` has already been consumed. Bound cumulative metadata as well as each
/// tag, so an arbitrary number of consecutive tags cannot delay audio forever.
pub(super) fn read(input: &mut dyn ReadSeek, consumed: &mut usize) -> Result<Option<i64>> {
    let mut header = [0; 7];
    input.read_exact(&mut header)?;
    let (version, flags) = (header[0], header[2]);
    if !matches!(version, 3 | 4) {
        return Err(Error::unsupported(
            "hls: packed audio requires ID3v2.3 or ID3v2.4",
        ));
    }
    if header[1] == 255 || flags & (if version == 3 { 0x1f } else { 0x0f }) != 0 {
        return Err(invalid());
    }
    let size = synchsafe(&header[3..])?;
    let footer = version == 4 && flags & 0x10 != 0;
    *consumed = consumed
        .checked_add(size + 10 + if footer { 10 } else { 0 })
        .ok_or_else(invalid)?;
    if *consumed > MAX_METADATA_BYTES {
        return Err(Error::unsupported(
            "hls: packed audio ID3 metadata exceeds 1 MiB",
        ));
    }
    let mut body = vec![0; size];
    input.read_exact(&mut body)?;
    if footer {
        let mut tail = [0; 10];
        input.read_exact(&mut tail)?;
        if &tail[..3] != b"3DI" || tail[3..] != header {
            return Err(invalid());
        }
    }
    if version == 3 && flags & 0x80 != 0 {
        body = deunsync(&body);
    }
    let mut frames = body.as_slice();
    if flags & 0x40 != 0 {
        let size_bytes = frames.get(..4).ok_or_else(invalid)?;
        let extended = if version == 3 {
            be_size(size_bytes)?.checked_add(4).ok_or_else(invalid)?
        } else {
            synchsafe(size_bytes)?
        };
        if extended < if version == 3 { 10 } else { 6 } {
            return Err(invalid());
        }
        frames = frames.get(extended..).ok_or_else(invalid)?;
    }
    let mut timestamp = None;
    while !frames.is_empty() {
        if frames[0] == 0 {
            if frames.iter().any(|b| *b != 0) {
                return Err(invalid());
            }
            break;
        }
        let h = frames.get(..10).ok_or_else(invalid)?;
        if h[..4]
            .iter()
            .any(|b| !b.is_ascii_uppercase() && !b.is_ascii_digit())
        {
            return Err(invalid());
        }
        let length = if version == 3 {
            be_size(&h[4..8])?
        } else {
            synchsafe(&h[4..8])?
        };
        let end = 10usize.checked_add(length).ok_or_else(invalid)?;
        let payload = frames.get(10..end).ok_or_else(invalid)?;
        if &h[..4] == b"PRIV" {
            let format = h[9];
            let allowed = if version == 3 { 0x20 } else { 0x43 };
            if format & !allowed != 0 {
                return Err(Error::unsupported(
                    "hls: compressed/encrypted ID3 PRIV frames are unsupported",
                ));
            }
            let decoded;
            let mut payload = if version == 4 && (format & 2 != 0 || flags & 0x80 != 0) {
                decoded = deunsync(payload);
                decoded.as_slice()
            } else {
                payload
            };
            if format & (if version == 3 { 0x20 } else { 0x40 }) != 0 {
                payload = payload.get(1..).ok_or_else(invalid)?;
            }
            if version == 4 && format & 1 != 0 {
                let size = synchsafe(payload.get(..4).ok_or_else(invalid)?)?;
                payload = &payload[4..];
                if size != payload.len() {
                    return Err(invalid());
                }
            }
            let nul = payload.iter().position(|b| *b == 0).ok_or_else(invalid)?;
            if &payload[..nul] == OWNER {
                let bytes: [u8; 8] = payload[nul + 1..].try_into().map_err(|_| invalid())?;
                let pts = u64::from_be_bytes(bytes);
                if pts >= 1 << 33 {
                    return Err(invalid());
                }
                if timestamp.is_some_and(|previous| previous != pts as i64) {
                    return Err(Error::invalid("hls: conflicting ID3 transport timestamps"));
                }
                timestamp = Some(pts as i64);
            }
        }
        frames = &frames[end..];
    }
    Ok(timestamp)
}
