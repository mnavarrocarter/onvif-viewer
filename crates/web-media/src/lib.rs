//! GTK-independent, shared RTSP-to-WebRTC media for a trusted HTTP server.
//!
//! Calls can block on GStreamer and must run on a blocking worker, not an async
//! executor or GTK thread. Keys identify a camera/profile and MUST NOT contain
//! credentials. A key must not be reused for different sources while in use.
//! SDP contains network addresses and must only be exchanged with authorized
//! clients. This crate does not implement HTTP, authentication, or TLS.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Condvar, Mutex, Weak, mpsc},
    time::{Duration, Instant},
};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_sdp as sdp;
use gstreamer_webrtc as webrtc;

const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SDP: usize = 256 * 1024;
const VIDEO_TRANSCODE: &str = "decodebin ! videoconvert ! videoscale ! videorate ! video/x-raw,format=I420,width=(int)[16,1280],height=(int)[16,720],framerate=(fraction)[0/1,30/1] ! x264enc tune=zerolatency speed-preset=ultrafast key-int-max=60 bitrate=2000 ! video/x-h264,profile=constrained-baseline ! h264parse name=web_output config-interval=-1";
const AUDIO_TRANSCODE: &str =
    "decodebin ! audioconvert ! audioresample ! opusenc name=web_output bitrate=64000";

/// The camera URI can itself contain credentials. Never log this value.
pub struct Request {
    pub uri: String,
    pub username: String,
    pub password: String,
}

/// An error safe to return to an authenticated client; no source URI or SDP is included.
#[derive(Debug, Clone)]
pub struct Error(String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
fn error(message: impl Into<String>) -> Error {
    Error(message.into())
}

// RFC 7587 RTP Opus is 48 kHz and advertises two channels even for mono.
// Nonstandard or underspecified camera caps retain the decoder/encoder path.
fn browser_opus_rtp(caps: &gst::StructureRef) -> bool {
    caps.get::<i32>("clock-rate").ok() == Some(48_000)
        && caps.get::<&str>("encoding-params").ok() == Some("2")
}

// rtph264pay may emit STAP-A, which RFC 6184 permits only in mode 1.
fn browser_rtp_compatible(caps: &gst::StructureRef, codec: &str) -> bool {
    caps.get::<&str>("encoding-name")
        .ok()
        .is_some_and(|name| name.eq_ignore_ascii_case(codec))
        && (codec != "H264" || caps.get::<&str>("packetization-mode").ok() == Some("1"))
}
fn outbound_caps(caps: &gst::Caps, codec: &str) -> gst::Caps {
    let mut result = caps.clone();
    if codec == "H264" {
        // 42c0 and 42e0 both describe constrained-baseline, but caps
        // compare profile-level-id byte-for-byte rather than by profile.
        result
            .make_mut()
            .structure_mut(0)
            .unwrap()
            .remove_field("profile-level-id");
    }
    result
}

fn media_decoder_bin(depay: &str, tail: &str) -> Result<gst::Bin, String> {
    let bin = gst::parse::bin_from_description(&format!("{depay} name=web_depay ! {tail}"), false)
        .map_err(|_| {
            "Could not construct the camera media decoder; check depayloader and decoder plugins."
                .to_owned()
        })?;
    for (element, pad, name) in [("web_depay", "sink", "sink"), ("web_output", "src", "src")] {
        let target = bin
            .by_name(element)
            .and_then(|element| element.static_pad(pad))
            .ok_or_else(|| "The camera media decoder is missing a pad.".to_owned())?;
        let ghost = gst::GhostPad::builder_with_target(&target)
            .map_err(|_| "Could not configure the camera media decoder pad.".to_owned())?
            .name(name)
            .build();
        bin.add_pad(&ghost)
            .map_err(|_| "Could not add the camera media decoder pad.".to_owned())?;
    }
    Ok(bin)
}

/// Owns the registry of shared ingests. Dropping it does not stop retained streams.
pub struct MediaManager {
    streams: Mutex<HashMap<String, Weak<Inner>>>,
    max_streams: usize,
    max_peers: usize,
}

impl MediaManager {
    pub fn new(max_streams: usize, max_peers_per_stream: usize) -> Result<Self, Error> {
        if max_streams == 0 || max_peers_per_stream == 0 {
            return Err(error("Media limits must be greater than zero."));
        }
        gst::init().map_err(|_| error("Could not initialize GStreamer."))?;
        Ok(Self {
            streams: Mutex::new(HashMap::new()),
            max_streams,
            max_peers: max_peers_per_stream,
        })
    }

    /// Reuses an existing ingest for `key`, otherwise starts a new one. The
    /// supplied request is ignored when the key already exists.
    pub fn open(&self, key: &str, request: Request) -> Result<Stream, Error> {
        let mut streams = self.streams.lock().unwrap();
        streams.retain(|_, value| value.strong_count() > 0);
        if let Some(inner) = streams.get(key).and_then(Weak::upgrade) {
            return Ok(Stream { inner });
        }
        if streams.len() >= self.max_streams {
            return Err(error("The active camera stream limit has been reached."));
        }
        for name in [
            "rtspsrc",
            "decodebin",
            "queue",
            "tee",
            "fakesink",
            "videoconvert",
            "videoscale",
            "videorate",
            "x264enc",
            "h264parse",
            "audioconvert",
            "audioresample",
            "opusenc",
            "rtph264pay",
            "rtpopuspay",
            "webrtcbin",
        ] {
            if gst::ElementFactory::find(name).is_none() {
                return Err(error(format!(
                    "Missing GStreamer element {name}; install the corresponding GStreamer plugin."
                )));
            }
        }
        let inner = Arc::new(Inner::start(request, self.max_peers)?);
        streams.insert(key.to_owned(), Arc::downgrade(&inner));
        Ok(Stream { inner })
    }
}

#[derive(Default)]
struct Tracks {
    video: bool,
    audio: bool,
    unsupported_audio: bool,
    ready: bool,
    failed: Option<String>,
}
struct Inner {
    pipeline: gst::Pipeline,
    video: gst::Element,
    audio: gst::Element,
    tracks: Arc<(Mutex<Tracks>, Condvar)>,
    peers: Mutex<usize>,
    failure: Arc<Mutex<Option<Error>>>,
    max_peers: usize,
}
impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Inner {
    fn start(request: Request, max_peers: usize) -> Result<Self, Error> {
        let pipeline = gst::parse::launch(
            "rtspsrc name=source latency=500 drop-on-latency=true protocols=tcp \
             tee name=video allow-not-linked=true tee name=audio allow-not-linked=true",
        )
        .map_err(|_| error("Could not construct the camera pipeline. Check GStreamer plugins."))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| error("Could not construct the camera pipeline."))?;
        let failure = Arc::new(Mutex::new(None));
        let reported_failure = Arc::clone(&failure);
        pipeline.bus().ok_or_else(|| error("Could not monitor camera ingest."))?
            .set_sync_handler(move |_, message| {
                let failure = match message.view() {
                    gst::MessageView::Error(message) => {
                        let factory = message.src().and_then(|source| source.downcast_ref::<gst::Element>())
                            .and_then(|element| element.factory()).map(|factory| factory.name().to_string())
                            .unwrap_or_else(|| "GStreamer".to_owned());
                        let category = message.error().kind::<gst::StreamError>();
                        let reason = message.debug().and_then(|debug| {
                            ["not-negotiated", "not-linked", "not-supported", "flushing"]
                                .into_iter().find(|reason| debug.contains(reason))
                        });
                        Some(error(format!("Camera media failed in {factory} ({category:?}, {reason:?}). Check camera connectivity and GStreamer codec plugins.")))
                    },
                    gst::MessageView::Eos(_) => Some(error("Camera media ended.")),
                    _ => None,
                };
                if let Some(failure) = failure {
                    let mut reported = reported_failure.lock().unwrap();
                    if reported.is_none() { *reported = Some(failure); }
                }
                gst::BusSyncReply::Drop
            });
        let source = pipeline
            .by_name("source")
            .ok_or_else(|| error("Could not configure the camera source."))?;
        let video = pipeline
            .by_name("video")
            .ok_or_else(|| error("Could not configure video."))?;
        let audio = pipeline
            .by_name("audio")
            .ok_or_else(|| error("Could not configure audio."))?;
        source.set_property("location", &request.uri);
        source.set_property("user-id", &request.username);
        source.set_property("user-pw", &request.password);
        let tracks = Arc::new((Mutex::new(Tracks::default()), Condvar::new()));
        let pending = Arc::clone(&tracks);
        let weak_pipeline = pipeline.downgrade();
        let weak_video = video.downgrade();
        let weak_audio = audio.downgrade();
        source.connect_pad_added(move |source, pad| {
            let Some(caps) = pad.current_caps() else { return; };
            let Some(s) = caps.structure(0) else { return; };
            let media = s.get::<&str>("media").unwrap_or("");
            let codec = s.get::<&str>("encoding-name").unwrap_or("");
            let depay = match (media, codec) {
                ("video", "H264") => "rtph264depay",
                ("video", "H265") => "rtph265depay",
                ("video", "JPEG") => "rtpjpegdepay",
                ("audio", "OPUS") => "rtpopusdepay",
                ("audio", "PCMA") => "rtppcmadepay",
                ("audio", "PCMU") => "rtppcmudepay",
                ("audio", "MPEG4-GENERIC") => "rtpmp4gdepay",
                _ => {
                    if media == "audio" { pending.0.lock().unwrap().unsupported_audio = true; }
                    return;
                },
            };
            let (lock, ready) = &*pending;
            let mut state = lock.lock().unwrap();
            if (media == "video" && state.video) || (media == "audio" && state.audio) { return; }
            let tail = if media == "video" {
                VIDEO_TRANSCODE
            } else if codec == "OPUS" && browser_opus_rtp(s) && gst::ElementFactory::find("opusparse").is_some() {
                "opusparse name=web_output"
            } else {
                AUDIO_TRANSCODE
            };
            let result = (|| -> Result<(), String> {
                let pipeline = weak_pipeline.upgrade().ok_or("The camera media pipeline closed.")?;
                let tee = if media == "video" { weak_video.upgrade() } else { weak_audio.upgrade() }
                    .ok_or("The camera media output closed.")?;
                let bin = media_decoder_bin(depay, tail)?;
                pipeline.add(&bin).map_err(|_| "Could not add the camera media decoder.")?;
                bin.link(&tee).map_err(|_| "Could not link the camera media decoder to its output.")?;
                bin.sync_state_with_parent().map_err(|_| "Could not start the camera media decoder.")?;
                let sink = bin.static_pad("sink").ok_or("The camera media decoder has no sink pad.")?;
                pad.link(&sink).map_err(|_| format!("Could not connect the camera RTP stream to {depay}; check camera codec settings."))?;
                Ok(())
            })();
            if let Err(message) = result {
                state.failed = Some(message);
                ready.notify_all();
                gst::element_error!(source, gst::StreamError::Failed, ("Could not decode camera media. Check the camera codec and GStreamer decoder plugins."));
            } else if media == "video" { state.video = true; } else { state.audio = true; }
        });
        let pending = Arc::clone(&tracks);
        source.connect_no_more_pads(move |_| {
            let (lock, ready) = &*pending;
            lock.lock().unwrap().ready = true;
            ready.notify_all();
        });
        if pipeline.set_state(gst::State::Playing).is_err() {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(error(
                "Could not start camera ingest. Check the camera and GStreamer plugins.",
            ));
        }
        Ok(Self {
            pipeline,
            video,
            audio,
            tracks,
            peers: Mutex::new(0),
            failure,
            max_peers,
        })
    }

    fn check(&self) -> Result<(), Error> {
        match self.failure.lock().unwrap().as_ref() {
            Some(failure) => Err(failure.clone()),
            None => Ok(()),
        }
    }
}

/// A handle to one camera/profile ingest. Clones and peers keep it alive.
#[derive(Clone)]
pub struct Stream {
    inner: Arc<Inner>,
}
impl Stream {
    /// Check for a pipeline failure. Failures remain visible to all viewers.
    pub fn check(&self) -> Result<(), Error> {
        self.inner.check()
    }

    /// Consume a fully gathered browser offer and return a fully gathered answer.
    /// The browser must offer H.264 video and, for audio, Opus receive transceivers.
    /// No trickle ICE is required. Keep the returned peer for the HTTP session.
    pub fn add_peer(&self, offer_sdp: &str) -> Result<(Peer, String), Error> {
        if offer_sdp.len() > MAX_SDP {
            return Err(error("SDP offer is too large."));
        }
        let offer = sdp::SDPMessage::parse_buffer(offer_sdp.as_bytes())
            .map_err(|_| error("Invalid SDP offer."))?;
        if offer.medias_len() > 8 {
            return Err(error("SDP offer has too many media sections."));
        }
        let deadline = Instant::now() + TIMEOUT;
        let (lock, ready) = &*self.inner.tracks;
        let mut tracks = lock.lock().unwrap();
        while !tracks.ready && tracks.failed.is_none() && Instant::now() < deadline {
            self.inner.check()?;
            let (next, _) = ready
                .wait_timeout(tracks, Duration::from_millis(100))
                .unwrap();
            tracks = next;
        }
        if let Some(message) = tracks.failed.as_ref() {
            return Err(error(message.clone()));
        }
        if !tracks.ready {
            return Err(error(
                "Timed out waiting for camera tracks. Check camera connectivity and credentials.",
            ));
        }
        if !tracks.video {
            return Err(error(
                "Camera has no supported video track (H.264, H.265, or JPEG).",
            ));
        }
        if tracks.unsupported_audio && !tracks.audio {
            return Err(error(
                "Camera audio codec is unsupported; use Opus, AAC, PCMA, or PCMU.",
            ));
        }
        let has_audio = tracks.audio;
        drop(tracks);
        self.inner.check()?;
        {
            let mut peers = self.inner.peers.lock().unwrap();
            if *peers >= self.inner.max_peers {
                return Err(error("The camera viewer limit has been reached."));
            }
            *peers += 1;
        }
        let peer = Peer::new(Arc::clone(&self.inner))?;
        let description =
            webrtc::WebRTCSessionDescription::new(webrtc::WebRTCSDPType::Offer, offer.clone());
        signal_promise(
            &peer.webrtc,
            "set-remote-description",
            Some(&description),
            false,
        )?;
        let mut video_added = false;
        let mut audio_added = false;
        for (index, media) in offer.medias().enumerate() {
            let (codec, tee) = match media.media() {
                Some("video") if !video_added => ("H264", &self.inner.video),
                Some("audio") if has_audio && !audio_added => ("OPUS", &self.inner.audio),
                _ => continue,
            };
            if media.port() == 0 {
                continue;
            }
            let mut caps = None;
            for candidate in media
                .formats()
                .filter_map(|f| f.parse::<i32>().ok())
                .filter_map(|pt| media.caps_from_media(pt))
            {
                let Some(format) = candidate.structure(0) else {
                    continue;
                };
                if !browser_rtp_compatible(format, codec) {
                    continue;
                }
                let preferred = codec != "H264"
                    || format.get::<&str>("profile").ok() == Some("constrained-baseline");
                if caps.is_none() || preferred {
                    caps = Some(candidate);
                }
                if preferred {
                    break;
                }
            }
            if let Some(mut caps) = caps {
                caps.make_mut()
                    .structure_mut(0)
                    .unwrap()
                    .set_name("application/x-rtp");
                peer.attach(tee, codec, index as u32, &caps)?;
                if codec == "H264" {
                    video_added = true;
                } else {
                    audio_added = true;
                }
            }
        }
        if !video_added {
            return Err(error(
                "The browser did not offer H.264 with packetization mode 1.",
            ));
        }
        if has_audio && !audio_added {
            return Err(error("The browser did not offer Opus audio."));
        }
        let (gathered_tx, gathered_rx) = mpsc::sync_channel(1);
        let notify = peer
            .webrtc
            .connect_notify(Some("ice-gathering-state"), move |webrtc, _| {
                if webrtc.property::<webrtc::WebRTCICEGatheringState>("ice-gathering-state")
                    == webrtc::WebRTCICEGatheringState::Complete
                {
                    let _ = gathered_tx.try_send(());
                }
            });
        let answer = signal_promise(&peer.webrtc, "create-answer", None, true)?
            .ok_or_else(|| error("WebRTC did not produce an answer."))?;
        signal_promise(&peer.webrtc, "set-local-description", Some(&answer), false)?;
        let gathered = gathered_rx.recv_timeout(TIMEOUT);
        peer.webrtc.disconnect(notify);
        gathered.map_err(|_| error("Timed out gathering WebRTC network candidates."))?;
        let local = peer
            .webrtc
            .property::<Option<webrtc::WebRTCSessionDescription>>("local-description")
            .ok_or_else(|| error("WebRTC did not produce a local description."))?;
        let text = local
            .sdp()
            .as_text()
            .map_err(|_| error("Could not serialize the WebRTC answer."))?;
        Ok((peer, text))
    }
}

fn signal_promise(
    element: &gst::Element,
    signal: &str,
    description: Option<&webrtc::WebRTCSessionDescription>,
    answer: bool,
) -> Result<Option<webrtc::WebRTCSessionDescription>, Error> {
    let (tx, rx) = mpsc::sync_channel(1);
    let promise = gst::Promise::with_change_func(move |reply| {
        let result = reply
            .map_err(|_| error("WebRTC negotiation failed."))
            .and_then(|reply| {
                if let Some(reply) = reply {
                    if reply.has_field("error") {
                        return Err(error(
                            "WebRTC negotiation failed; check the browser offer and media codecs.",
                        ));
                    }
                    if answer {
                        return reply
                            .get::<webrtc::WebRTCSessionDescription>("answer")
                            .map(Some)
                            .map_err(|_| error("WebRTC did not produce an answer."));
                    }
                }
                if answer {
                    Err(error("WebRTC did not produce an answer."))
                } else {
                    Ok(None)
                }
            });
        let _ = tx.send(result);
    });
    if let Some(description) = description {
        element.emit_by_name::<()>(signal, &[description, &promise]);
    } else {
        element.emit_by_name::<()>(signal, &[&None::<gst::Structure>, &promise]);
    }
    match rx.recv_timeout(TIMEOUT) {
        Ok(result) => result,
        Err(_) => {
            promise.interrupt();
            Err(error("Timed out negotiating WebRTC."))
        }
    }
}

struct Branch {
    tee: gst::Element,
    tee_pad: gst::Pad,
    sink_pad: gst::Pad,
    bin: gst::Bin,
}
/// A single browser connection. Drop it to detach without stopping other viewers.
pub struct Peer {
    inner: Arc<Inner>,
    webrtc: gst::Element,
    branches: Mutex<Vec<Branch>>,
}
impl Peer {
    fn new(inner: Arc<Inner>) -> Result<Self, Error> {
        let webrtc = match gst::ElementFactory::make("webrtcbin").build() {
            Ok(element) => element,
            Err(_) => {
                *inner.peers.lock().unwrap() -= 1;
                return Err(error(
                    "Could not create WebRTC; install GStreamer WebRTC and ICE plugins.",
                ));
            }
        };
        webrtc.set_property_from_str("bundle-policy", "max-bundle");
        if inner.pipeline.add(&webrtc).is_err() {
            *inner.peers.lock().unwrap() -= 1;
            return Err(error("Could not add the WebRTC connection."));
        }
        let peer = Self {
            inner,
            webrtc,
            branches: Mutex::new(Vec::new()),
        };
        peer.webrtc
            .sync_state_with_parent()
            .map_err(|_| error("Could not start the WebRTC connection. Check ICE plugins."))?;
        Ok(peer)
    }
    fn attach(
        &self,
        tee: &gst::Element,
        codec: &str,
        index: u32,
        caps: &gst::Caps,
    ) -> Result<(), Error> {
        let pt = caps
            .structure(0)
            .and_then(|s| s.get::<i32>("payload").ok())
            .ok_or_else(|| error("Browser SDP is missing a payload type."))?;
        let pay = if codec == "H264" {
            "rtph264pay config-interval=-1 aggregate-mode=zero-latency"
        } else {
            "rtpopuspay"
        };
        let transceiver = self
            .webrtc
            .emit_by_name::<Option<webrtc::WebRTCRTPTransceiver>>(
                "get-transceiver",
                &[&(index as i32)],
            )
            .ok_or_else(|| error("Could not find the browser's WebRTC transceiver."))?;
        transceiver.set_property("direction", webrtc::WebRTCRTPTransceiverDirection::Sendonly);
        let bin = gst::parse::bin_from_description(&format!("queue max-size-buffers=30 max-size-bytes=0 max-size-time=0 leaky=downstream ! {pay} pt={pt} ! capsfilter name=filter"), true)
            .map_err(|_| error("Could not construct a WebRTC media branch. Check GStreamer RTP plugins."))?;
        let filter_caps = outbound_caps(caps, codec);
        bin.by_name("filter")
            .ok_or_else(|| error("Could not configure RTP caps."))?
            .set_property("caps", &filter_caps);
        self.inner
            .pipeline
            .add(&bin)
            .map_err(|_| error("Could not add a WebRTC media branch."))?;
        let tee_pad = match tee.request_pad_simple("src_%u") {
            Some(p) => p,
            None => {
                let _ = self.inner.pipeline.remove(&bin);
                return Err(error("Could not attach a camera track."));
            }
        };
        let sink_pad = match self.webrtc.request_pad_simple(&format!("sink_{index}")) {
            Some(p) => p,
            None => {
                tee.release_request_pad(&tee_pad);
                let _ = self.inner.pipeline.remove(&bin);
                return Err(error("Could not attach a WebRTC track."));
            }
        };
        self.branches.lock().unwrap().push(Branch {
            tee: tee.clone(),
            tee_pad: tee_pad.clone(),
            sink_pad: sink_pad.clone(),
            bin: bin.clone(),
        });
        bin.static_pad("src")
            .ok_or_else(|| error("Missing RTP source pad."))?
            .link(&sink_pad)
            .map_err(|reason| {
                error(format!("Could not link the WebRTC {codec} track: {reason}"))
            })?;
        let branch_sink = bin
            .static_pad("sink")
            .ok_or_else(|| error("Missing RTP sink pad."))?;
        tee_pad.link(&branch_sink).map_err(|reason| {
            error(format!("Could not link the {codec} camera track: {reason}"))
        })?;
        bin.sync_state_with_parent()
            .map_err(|_| error("Could not start the WebRTC media branch."))?;
        Ok(())
    }
    /// True after WebRTC reports failure or closure. Disconnected may be transient.
    pub fn is_closed(&self) -> bool {
        matches!(
            self.webrtc
                .property::<webrtc::WebRTCPeerConnectionState>("connection-state"),
            webrtc::WebRTCPeerConnectionState::Failed | webrtc::WebRTCPeerConnectionState::Closed
        )
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        for branch in self.branches.lock().unwrap().drain(..) {
            let _ = branch.tee_pad.push_event(gst::event::FlushStart::new());
            if let Some(sink) = branch.bin.static_pad("sink") {
                let _ = branch.tee_pad.unlink(&sink);
            }
            if let Some(src) = branch.bin.static_pad("src") {
                let _ = src.unlink(&branch.sink_pad);
            }
            branch.tee.release_request_pad(&branch.tee_pad);
            self.webrtc.release_request_pad(&branch.sink_pad);
            let _ = branch.bin.set_state(gst::State::Null);
            let _ = self.inner.pipeline.remove(&branch.bin);
        }
        let _ = self.webrtc.set_state(gst::State::Null);
        let _ = self.inner.pipeline.remove(&self.webrtc);
        *self.inner.peers.lock().unwrap() -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constrained_baseline_sender_accepts_equivalent_browser_profile() {
        gst::init().unwrap();
        let pipeline = gst::parse::launch(
            "videotestsrc num-buffers=8 ! videoconvert ! video/x-raw,width=1280,height=720,framerate=25/1 ! x264enc tune=zerolatency speed-preset=ultrafast ! video/x-h264,profile=constrained-baseline ! h264parse ! rtph264pay pt=102 ! capsfilter name=filter ! fakesink sync=false"
        ).unwrap().downcast::<gst::Pipeline>().unwrap();
        let offer = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "H264")
            .field("clock-rate", 90_000i32)
            .field("payload", 102i32)
            .field("packetization-mode", "1")
            .field("profile", "constrained-baseline")
            .field("profile-level-id", "42e01f")
            .build();
        pipeline
            .by_name("filter")
            .unwrap()
            .set_property("caps", outbound_caps(&offer, "H264"));
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("RTP output did not finish");
        if let gst::MessageView::Error(error) = message.view() {
            panic!("RTP negotiation failed: {}", error.error());
        }
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn rtp_decoders_accept_audio_and_video_with_dynamic_decodebin() {
        gst::init().unwrap();
        for (source, depay, tail) in [
            (
                "audiotestsrc num-buffers=20 ! audioconvert ! mulawenc ! rtppcmupay name=rtp",
                "rtppcmudepay",
                AUDIO_TRANSCODE,
            ),
            (
                "videotestsrc num-buffers=20 ! videoconvert ! x264enc tune=zerolatency ! rtph264pay name=rtp",
                "rtph264depay",
                VIDEO_TRANSCODE,
            ),
        ] {
            let pipeline = gst::parse::launch(&format!("{source} fakesink name=output sync=false"))
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();
            let decoder = media_decoder_bin(depay, tail).unwrap();
            pipeline.add(&decoder).unwrap();
            let input = pipeline.by_name("rtp").unwrap();
            let output = pipeline.by_name("output").unwrap();
            gst::Element::link_many([&input, decoder.upcast_ref(), &output]).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            let message = pipeline
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(15),
                    &[gst::MessageType::Error, gst::MessageType::Eos],
                )
                .expect("media pipeline did not finish");
            if let gst::MessageView::Error(error) = message.view() {
                panic!("media pipeline failed: {}", error.error());
            }
            pipeline.set_state(gst::State::Null).unwrap();
        }
    }

    #[test]
    fn h264_negotiation_skips_incompatible_packetization_mode() {
        gst::init().unwrap();
        let offer = b"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 104 102\r\na=rtpmap:104 H264/90000\r\na=fmtp:104 packetization-mode=0;profile-level-id=42001f\r\na=rtpmap:102 H264/90000\r\na=fmtp:102 packetization-mode=1;profile-level-id=42001f\r\n";
        let offer = sdp::SDPMessage::parse_buffer(offer).unwrap();
        let media = offer.media(0).unwrap();
        let selected = media
            .formats()
            .filter_map(|f| f.parse::<i32>().ok())
            .filter_map(|pt| media.caps_from_media(pt))
            .find(|caps| {
                caps.structure(0)
                    .is_some_and(|s| browser_rtp_compatible(s, "H264"))
            })
            .unwrap();
        assert_eq!(
            selected
                .structure(0)
                .unwrap()
                .get::<i32>("payload")
                .unwrap(),
            102
        );
    }

    #[test]
    fn opus_passthrough_requires_standard_rtp_parameters() {
        gst::init().unwrap();
        let standard = gst::Structure::builder("application/x-rtp")
            .field("clock-rate", 48_000i32)
            .field("encoding-params", "2")
            .build();
        assert!(browser_opus_rtp(&standard));
        let nonstandard = gst::Structure::builder("application/x-rtp")
            .field("clock-rate", 8_000i32)
            .field("encoding-params", "1")
            .build();
        assert!(!browser_opus_rtp(&nonstandard));
        let missing = gst::Structure::builder("application/x-rtp").build();
        assert!(!browser_opus_rtp(&missing));
    }
}
