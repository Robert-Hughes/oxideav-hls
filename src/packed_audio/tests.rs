use super::*;
use std::io::Cursor;

fn sync_size(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 127) as u8,
        ((n >> 14) & 127) as u8,
        ((n >> 7) & 127) as u8,
        (n & 127) as u8,
    ]
}

fn frame(id: &[u8; 4], payload: &[u8], version: u8, flags: u8) -> Vec<u8> {
    let mut b = id.to_vec();
    b.extend(if version == 4 {
        sync_size(payload.len())
    } else {
        (payload.len() as u32).to_be_bytes()
    });
    b.extend([0, flags]);
    b.extend(payload);
    b
}

fn tag(body: &[u8], version: u8, flags: u8) -> Vec<u8> {
    let mut b = b"ID3".to_vec();
    b.extend([version, 0, flags]);
    b.extend(sync_size(body.len()));
    b.extend(body);
    if flags & 0x10 != 0 && version == 4 {
        b.extend(b"3DI");
        b.extend([version, 0, flags]);
        b.extend(sync_size(body.len()));
    }
    b
}

fn timestamp_payload(pts: u64) -> Vec<u8> {
    let mut b = b"com.apple.streaming.transportStreamTimestamp\0".to_vec();
    b.extend(pts.to_be_bytes());
    b
}

pub(crate) fn timestamp_tag(pts: u64) -> Vec<u8> {
    tag(&frame(b"PRIV", &timestamp_payload(pts), 3, 0), 3, 0)
}

fn adts(rate: u8, channels: u8, crc: bool, blocks: u8) -> Vec<u8> {
    let n: usize = if crc { 13 } else { 11 };
    let mut b = vec![
        0xff,
        if crc { 0xf0 } else { 0xf1 },
        0x40 | (rate << 2) | (channels >> 2),
        ((channels & 3) << 6) | ((n >> 11) as u8),
        (n >> 3) as u8,
        ((n & 7) as u8) << 5 | 0x1f,
        0xfc | blocks,
    ];
    b.resize(n, 0);
    b
}

pub(crate) fn segment(pts: u64, rate: u8, frames: usize) -> Vec<u8> {
    let mut b = timestamp_tag(pts);
    for _ in 0..frames {
        b.extend(adts(rate, 2, false, 0));
    }
    b
}

fn demux(bytes: Vec<u8>) -> Result<Box<dyn Demuxer>> {
    open(Box::new(Cursor::new(bytes)), &Timeline::default(), 0.0)
}

fn read_tag(bytes: Vec<u8>) -> Result<Option<i64>> {
    let mut c = Cursor::new(bytes);
    c.set_position(3);
    id3::read(&mut c, &mut 0)
}

#[test]
fn adts_packets_preserve_payload_and_transport_origin_without_rounding_drift() {
    for rate in [3, 4, 8] {
        let mut d = demux(segment(9_000_001, rate, 1000)).unwrap();
        let hz = d.streams()[0].params.sample_rate.unwrap() as i64;
        assert_eq!(d.streams()[0].start_time, Some(9_000_001));
        assert_eq!(d.streams()[0].params.channels, Some(2));
        for i in 0..1000 {
            let p = d.next_packet().unwrap();
            assert_eq!(p.data, adts(rate, 2, false, 0));
            let pts = 9_000_001 + i * 1024 * 90_000 / hz;
            let end = 9_000_001 + (i + 1) * 1024 * 90_000 / hz;
            assert_eq!(
                (p.pts, p.dts, p.duration),
                (Some(pts), Some(pts), Some(end - pts))
            );
            assert_eq!(p.time_base, CLOCK);
            assert!(p.flags.keyframe);
        }
        assert!(matches!(d.next_packet(), Err(Error::Eof)));
    }
}

#[test]
fn crc_multiblock_and_channel_configuration_are_preserved() {
    for (configuration, channels) in [(0, None), (1, Some(1)), (7, Some(8))] {
        let mut bytes = timestamp_tag(90_000);
        let audio = adts(3, configuration, true, 2);
        bytes.extend(&audio);
        let mut d = demux(bytes).unwrap();
        assert_eq!(d.streams()[0].params.channels, channels);
        let p = d.next_packet().unwrap();
        assert_eq!(p.data, audio);
        assert_eq!(p.duration, Some(5760));
    }
}

#[test]
fn seeks_land_on_frame_before_target_and_can_repeat_after_eof() {
    let mut d = demux(segment(900_000, 3, 4)).unwrap();
    for (target, expected) in [
        (902_000, 901_920),
        (0, 900_000),
        (999_999, 905_760),
        (903_840, 903_840),
    ] {
        assert_eq!(d.seek_to(0, target).unwrap(), expected);
        assert_eq!(d.next_packet().unwrap().pts, Some(expected));
    }
    assert!(d.seek_to(1, 900_000).is_err());
}

#[test]
fn timeline_unwraps_deterministically_across_seeks_and_multiple_wraps() {
    let t = Timeline::default();
    let c = AdtsConfig {
        mpeg2: false,
        profile: 1,
        sample_rate: 48000,
        channels: 2,
    };
    assert_eq!(t.unwrap(WRAP - 90_000, 0.0, c).unwrap(), WRAP - 90_000);
    assert_eq!(t.unwrap(90_000, 2.0, c).unwrap(), WRAP + 90_000);
    assert_eq!(
        t.unwrap(0, 1.0 + WRAP as f64 / 90_000.0, c).unwrap(),
        2 * WRAP
    );
    assert_eq!(t.unwrap(0, 1.0, c).unwrap(), WRAP);
    assert_eq!(t.unwrap(WRAP - 90_000, 0.0, c).unwrap(), WRAP - 90_000);
    assert!(t
        .unwrap(
            0,
            1.0,
            AdtsConfig {
                sample_rate: 44100,
                ..c
            }
        )
        .is_err());
}

#[test]
fn id3_versions_unknown_frames_padding_extended_headers_and_footer() {
    for v in [3, 4] {
        let mut body = if v == 3 {
            vec![0, 0, 0, 6, 0, 0, 0, 0, 0, 0]
        } else {
            vec![0, 0, 0, 6, 1, 0]
        };
        body.extend(frame(b"TXXX", &[1; 180], v, 0));
        body.extend(frame(b"PRIV", &timestamp_payload(123_456), v, 0));
        body.extend([0; 10]);
        assert_eq!(read_tag(tag(&body, v, 0x40)).unwrap(), Some(123_456));
    }
    let body = frame(b"PRIV", &timestamp_payload(1), 4, 0);
    assert_eq!(read_tag(tag(&body, 4, 0x10)).unwrap(), Some(1));
}

fn unsync(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for byte in b {
        out.push(*byte);
        if *byte == 255 {
            out.push(0);
        }
    }
    out
}

#[test]
fn unsynchronised_priv_and_grouping_data_length_fields() {
    let payload = timestamp_payload(0x1_ff_ff_ff_ff);
    let v3 = frame(b"PRIV", &payload, 3, 0);
    assert_eq!(
        read_tag(tag(&unsync(&v3), 3, 0x80)).unwrap(),
        Some(WRAP - 1)
    );
    for flags in [0, 0x80] {
        let mut p = vec![1];
        p.extend(sync_size(payload.len()));
        p.extend(&payload);
        let v4 = frame(b"PRIV", &unsync(&p), 4, 0x43);
        assert_eq!(read_tag(tag(&v4, 4, flags)).unwrap(), Some(WRAP - 1));
    }
}

#[test]
fn consecutive_and_interleaved_metadata_do_not_become_audio_packets() {
    let empty_tag = tag(&frame(b"TXXX", b"metadata", 3, 0), 3, 0);
    let mut bytes = empty_tag.clone();
    bytes.extend(segment(42, 3, 1));
    bytes.extend(empty_tag);
    bytes.extend(adts(3, 2, false, 0));
    let mut d = demux(bytes).unwrap();
    assert_eq!(d.next_packet().unwrap().pts, Some(42));
    assert_eq!(d.next_packet().unwrap().pts, Some(42 + 1920));
    assert!(matches!(d.next_packet(), Err(Error::Eof)));
}

#[test]
fn malformed_or_unsupported_metadata_and_audio_fail_explicitly() {
    let mut invalid = vec![
        adts(3, 2, false, 0), // missing ID3 timestamp
        timestamp_tag(1),     // no audio
        segment(1 << 33, 3, 1),
        segment(0, 15, 1), // reserved frequency
    ];
    let mut conflict = timestamp_tag(1);
    conflict.extend(segment(2, 3, 1));
    invalid.push(conflict);
    let mut unknown = timestamp_tag(1);
    unknown.extend(b"unsupported codec");
    invalid.push(unknown);
    let mut truncated = segment(0, 3, 1);
    truncated.pop();
    invalid.push(truncated);
    let mut bad_size = segment(0, 3, 1);
    bad_size[6] = 0x80;
    invalid.push(bad_size);
    let mut excessive = b"ID3\x03\0\0".to_vec();
    excessive.extend(sync_size(2 * 1024 * 1024));
    invalid.push(excessive);
    let mut compressed = tag(&frame(b"PRIV", &timestamp_payload(1), 4, 8), 4, 0);
    compressed.extend(adts(3, 2, false, 0));
    invalid.push(compressed);
    for bytes in invalid {
        assert!(demux(bytes).is_err());
    }

    let mut bytes = segment(0, 3, 1);
    bytes.extend(adts(4, 2, false, 0));
    let mut d = demux(bytes).unwrap();
    d.next_packet().unwrap();
    assert!(d.next_packet().is_err());
    let mut bytes = segment(0, 3, 1);
    bytes.extend([255, 241]);
    let mut d = demux(bytes).unwrap();
    d.next_packet().unwrap();
    assert!(d.next_packet().is_err());
}
