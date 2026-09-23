//! HLS segment-format dispatch. Probe bytes, never URL suffixes or hostnames.

use crate::packed_audio::{self, Timeline};
use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek, Result};
use std::io::{Read, SeekFrom};

#[derive(Debug, PartialEq, Eq)]
enum Format {
    MpegTs,
    Mp4,
    PackedAudio,
}

fn detect(prefix: &[u8], mapped: bool) -> Result<Format> {
    let format = if prefix.first() == Some(&0x47) && (prefix.len() <= 188 || prefix[188] == 0x47) {
        Format::MpegTs
    } else if matches!(prefix.get(4..8), Some(b"ftyp" | b"moov" | b"styp")) {
        Format::Mp4
    } else if prefix.starts_with(b"ID3") {
        Format::PackedAudio
    } else {
        return Err(Error::unsupported("hls: unrecognized segment format (expected MPEG-TS, mapped fMP4, or ID3-timestamped packed AAC)"));
    };
    match (&format, mapped) {
        (Format::Mp4, false) => Err(Error::invalid("hls: fMP4 segment requires EXT-X-MAP")),
        (Format::PackedAudio, true) => {
            Err(Error::invalid("hls: packed audio must not have EXT-X-MAP"))
        }
        _ => Ok(format),
    }
}

pub(crate) fn open(
    mut input: Box<dyn ReadSeek>,
    mapped: bool,
    timeline: &Timeline,
    start_seconds: f64,
) -> Result<Box<dyn Demuxer>> {
    let mut prefix = Vec::new();
    input.by_ref().take(3 * 188).read_to_end(&mut prefix)?;
    input.seek(SeekFrom::Start(0))?;
    match detect(&prefix, mapped)? {
        Format::MpegTs => oxideav_mpegts::open_demuxer(input, &NullCodecResolver),
        Format::Mp4 => oxideav_mp4::demux::open(input, &NullCodecResolver),
        Format::PackedAudio => packed_audio::open(input, timeline, start_seconds),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_uses_content_and_enforces_initialization_rules() {
        let mut ts = vec![0; 376];
        ts[0] = 0x47;
        ts[188] = 0x47;
        assert_eq!(detect(&ts, false).unwrap(), Format::MpegTs);
        assert_eq!(detect(&ts, true).unwrap(), Format::MpegTs);
        assert_eq!(detect(b"\0\0\0\x18ftyp", true).unwrap(), Format::Mp4);
        assert_eq!(detect(b"ID3\x03\0\0", false).unwrap(), Format::PackedAudio);
        assert!(detect(b"\0\0\0\x18ftyp", false).is_err());
        assert!(detect(b"ID3\x03\0\0", true).is_err());
        for bytes in [b"".as_slice(), b"WEBVTT", b"\xff\xf1", b"not media"] {
            assert!(detect(bytes, false).is_err());
        }
    }
}
