//! End-to-end tests of discovery and packet delivery over a local HTTP origin.
//! Fixtures are synthetic: no signed URLs, credentials, network services or codecs.
use super::*;
use crate::packed_audio::tests::segment;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};

struct Origin {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Origin {
    fn new(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                // Windows accepted sockets inherit the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let fields: Vec<_> = line.split_whitespace().collect();
                let method = fields[0].to_owned();
                let path = fields[1].to_owned();
                recorded.lock().unwrap().push(format!("{method} {path}"));
                let mut range = None;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.eq_ignore_ascii_case("range") {
                            range = value.trim().strip_prefix("bytes=").map(str::to_owned);
                        }
                    }
                }
                let Some(body) = files.get(&path) else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                };
                let (start, end, status) = if let Some(range) = range {
                    let (a, b) = range.split_once('-').unwrap();
                    (
                        a.parse::<usize>().unwrap(),
                        b.parse::<usize>()
                            .unwrap_or(body.len() - 1)
                            .min(body.len() - 1),
                        "206 Partial Content",
                    )
                } else {
                    (0, body.len() - 1, "200 OK")
                };
                let mut headers = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n", end - start + 1);
                if status.starts_with("206") {
                    headers.push_str(&format!(
                        "Content-Range: bytes {start}-{end}/{}\r\n",
                        body.len()
                    ));
                }
                headers.push_str("\r\n");
                let _ = stream.write_all(headers.as_bytes());
                if method != "HEAD" {
                    let _ = stream.write_all(&body[start..=end]);
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn hls(&self, path: &str) -> String {
        format!("hls+{}{path}", self.url)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let joined = self.worker.take().unwrap().join();
        if !thread::panicking() {
            joined.unwrap();
        }
    }
}

#[test]
fn discovery_preserves_audio_groups_and_metadata_without_fetching_renditions() {
    let master = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="stereo",NAME="English",LANGUAGE="en",ASSOC-LANGUAGE="en-GB",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",CHARACTERISTICS="public.accessibility.describes-video",URI="../audio/en.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="stereo",NAME="French",LANGUAGE="fr",URI="/audio/fr.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="surround",NAME="English",LANGUAGE="en",CHANNELS="6/JOC",URI="https://cdn.example.test/surround.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="embedded",NAME="Main",DEFAULT=YES,AUTOSELECT=YES
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="captions",NAME="English",URI="captions.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,AUDIO="stereo"
video.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1920x1080,AUDIO="surround"
hd.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=500000,AUDIO="embedded"
muxed.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=100000
silent.m3u8
"#;
    let origin = Origin::new(HashMap::from([(
        "/path/master.m3u8".into(),
        master.to_vec(),
    )]));
    let HlsPlaylistInfo::Master {
        variants,
        audio_renditions,
        ..
    } = inspect_hls(&origin.hls("/path/master.m3u8")).unwrap()
    else {
        panic!("master")
    };
    assert_eq!(audio_renditions.len(), 4);
    let matched: Vec<_> = variants[0].audio_renditions(&audio_renditions).collect();
    assert_eq!(matched.len(), 2);
    let en = matched[0];
    assert_eq!(
        en.url.as_ref().unwrap().as_str(),
        format!("{}/audio/en.m3u8", origin.url)
    );
    assert_eq!(en.language.as_deref(), Some("en"));
    assert_eq!(en.assoc_language.as_deref(), Some("en-GB"));
    assert_eq!(en.channels.as_deref(), Some("2"));
    assert_eq!(
        en.characteristics.as_deref(),
        Some("public.accessibility.describes-video")
    );
    assert!(en.default && en.autoselect);
    assert!(!matched[1].default && !matched[1].autoselect);
    assert_eq!(
        matched[1].url.as_ref().unwrap().as_str(),
        format!("{}/audio/fr.m3u8", origin.url)
    );
    let surround = variants[1]
        .audio_renditions(&audio_renditions)
        .next()
        .unwrap();
    assert_eq!(surround.channels.as_deref(), Some("6/JOC"));
    assert_eq!(
        surround.url.as_ref().unwrap().as_str(),
        "https://cdn.example.test/surround.m3u8"
    );
    assert!(variants[2]
        .audio_renditions(&audio_renditions)
        .next()
        .unwrap()
        .url
        .is_none());
    assert_eq!(variants[3].audio_renditions(&audio_renditions).count(), 0);
    assert_eq!(*origin.requests.lock().unwrap(), ["GET /path/master.m3u8"]);
}

#[test]
fn invalid_audio_uris_fail_discovery() {
    for uri in [
        "file:///audio.aac",
        "ftp://example.test/audio",
        "https://[",
        "",
    ] {
        let master = format!("#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Main\",URI=\"{uri}\"\n#EXT-X-STREAM-INF:BANDWIDTH=100,AUDIO=\"a\"\nv.m3u8\n");
        let (_, Playlist::MasterPlaylist(master)) =
            m3u8_rs::parse_playlist(master.as_bytes()).unwrap()
        else {
            panic!("master")
        };
        assert!(
            inspect_master(
                &Url::parse("https://example.test/master.m3u8").unwrap(),
                &master
            )
            .is_err(),
            "URI: {uri:?}, alternatives: {:?}",
            master.alternatives
        );
    }
}

#[test]
fn packed_aac_http_ranges_boundaries_wrap_and_repeated_seeks() {
    let wrap = 1u64 << 33;
    let first = wrap - 3840;
    let a = segment(first, 3, 2);
    let b = segment(0, 3, 2);
    let c = segment(3840, 3, 2);
    let mut resource = vec![99; 13];
    resource.extend(&a);
    resource.extend(&b);
    resource.extend(&c);
    resource.extend([99; 11]);
    let media = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-BYTERANGE:{}@13\n#EXTINF:0.042666667,\nopaque\n#EXT-X-BYTERANGE:{}\n#EXTINF:0.042666667,\nopaque\n#EXT-X-BYTERANGE:{}\n#EXTINF:0.042666667,\nopaque\n#EXT-X-ENDLIST\n", a.len(), b.len(), c.len());
    let origin = Origin::new(HashMap::from([
        ("/audio.m3u8".into(), media.into_bytes()),
        ("/opaque".into(), resource),
    ]));
    assert!(matches!(
        inspect_hls(&origin.hls("/audio.m3u8")).unwrap(),
        HlsPlaylistInfo::Media { .. }
    ));
    let mut source = open_hls(&origin.hls("/audio.m3u8")).unwrap();
    assert!(source.supports_seek());
    assert_eq!(source.streams()[0].start_time, Some(first as i64));
    for i in 0..6 {
        assert_eq!(
            source.next_packet().unwrap().pts,
            Some(first as i64 + i * 1920)
        );
    }
    assert!(matches!(source.next_packet(), Err(Error::Eof)));
    for (target, landed) in [
        (wrap + 2000, wrap + 1920),
        (first, first),
        (wrap + 4000, wrap + 3840),
    ] {
        assert_eq!(source.seek_to(0, target as i64).unwrap(), landed as i64);
        assert_eq!(source.next_packet().unwrap().pts, Some(landed as i64));
    }
    // Drain the final successor before dropping the mock origin.
    loop {
        match source.next_packet() {
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(error) => panic!("failed to drain final segment: {error}"),
        }
    }
}
