//! Opt-in smoke test for a caller-supplied master playlist. Never stores signed
//! URLs or media; demuxes packets and decodes a short prefix without playback.
use oxideav_core::{Error, Frame, MediaType, TimeBase};
use oxideav_hls::{inspect_hls, open_hls, HlsPlaylistInfo};

#[test]
#[ignore = "requires OXIDEAV_TEST_HLS_MASTER pointing to a completed VOD with external AAC"]
fn discover_read_and_seek_external_audio() {
    let uri = std::env::var("OXIDEAV_TEST_HLS_MASTER").expect("set OXIDEAV_TEST_HLS_MASTER");
    let config = oxideav_http::HttpConfig::builder()
        .range_probe(true)
        .build();
    oxideav_http::install_default_config(config).unwrap();
    let HlsPlaylistInfo::Master {
        variants,
        preferred_variant,
        audio_renditions,
    } = inspect_hls(&uri).unwrap()
    else {
        panic!("expected master")
    };
    let variant = &variants[preferred_variant];
    let audio = variant
        .audio_renditions(&audio_renditions)
        .filter(|r| r.url.is_some())
        .max_by_key(|r| (r.default, r.autoselect))
        .expect("external audio");
    let mut source = open_hls(&format!("hls+{}", audio.url.as_ref().unwrap())).unwrap();
    let info = source
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Audio)
        .unwrap()
        .clone();
    let mut first_pts = None;
    let mut last_pts = None;
    let mut decoder = oxideav_aac::codec_decoder::make_decoder(&info.params).unwrap();
    let mut decoded_frames = 0;
    for index in 0..600 {
        let p = source.next_packet().unwrap();
        if p.stream_index != info.index {
            continue;
        }
        let pts = p.pts.expect("audio PTS");
        assert!(last_pts.map_or(true, |previous| pts >= previous));
        first_pts.get_or_insert(pts);
        last_pts = Some(pts);
        if index < 20 {
            decoder.send_packet(&p).unwrap();
            loop {
                match decoder.receive_frame() {
                    Ok(Frame::Audio(_)) => decoded_frames += 1,
                    Err(Error::NeedMore) => break,
                    other => panic!("unexpected decoder result: {other:?}"),
                }
            }
        }
    }
    assert!(decoded_frames > 0);
    let start = info.start_time.unwrap_or_else(|| first_pts.unwrap());
    let target = start + TimeBase::new(1, 1).rescale(60, info.time_base);
    let landed = source.seek_to(info.index, target).unwrap();
    assert!(landed <= target);
    assert!(info.time_base.seconds_of(target - landed) < 10.0);
    let packet = source.next_packet().unwrap();
    assert_eq!(packet.pts, Some(landed));
    println!("audio codec={} rate={:?} channels={:?}; 600 packets from {:.3}s to {:.3}s; seek landed at {:.3}s",
        info.params.codec_id, info.params.sample_rate, info.params.channels,
        info.time_base.seconds_of(first_pts.unwrap()), info.time_base.seconds_of(last_pts.unwrap()), info.time_base.seconds_of(landed));
    println!("decoded {decoded_frames} audio frames without playback");

    // Exercise the unchanged MPEG-TS/fMP4 branch from the same master too.
    let mut video = open_hls(&format!("hls+{}", variant.url)).unwrap();
    assert!(video
        .streams()
        .iter()
        .any(|s| s.params.media_type == MediaType::Video));
    assert!(video.next_packet().unwrap().pts.is_some());
}
