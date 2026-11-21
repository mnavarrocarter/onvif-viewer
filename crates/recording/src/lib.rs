//! UI-independent local RTSP recording.
//!
//! Recordings are remuxed into Matroska instead of decoded and re-encoded.
//! This keeps supported camera video and audio compressed and makes this
//! component usable by a future background recorder as well as a visible feed.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use gst::prelude::*;
use gstreamer as gst;

const RTSP_LATENCY_MS: u32 = 500;
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Parameters for a new local recording. They are only retained by its worker.
pub struct Request {
    pub uri: String,
    pub username: String,
    pub password: String,
    pub destination: PathBuf,
}

/// State changes emitted by a recording worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Started,
    Finished,
    Failed(String),
}

enum Command {
    Stop,
}

/// A running recording controlled independently from a GTK window.
pub struct Recording {
    commands: Sender<Command>,
    events: Receiver<Event>,
}

impl Recording {
    /// Starts a dedicated GStreamer worker. The stream is not considered active
    /// until [`Event::Started`] is received.
    pub fn start(request: Request) -> Result<Self, String> {
        let (commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        thread::Builder::new()
            .name("camera-recording".into())
            .spawn(move || worker(request, commands_rx, events_tx))
            .map_err(|error| format!("Could not start the recording worker: {error}"))?;
        Ok(Self {
            commands: commands_tx,
            events: events_rx,
        })
    }

    /// Requests a clean stop. `Finished` means the container was finalized.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// Returns the next state change without blocking the caller.
    pub fn try_event(&self) -> Result<Event, TryRecvError> {
        self.events.try_recv()
    }
}

fn worker(request: Request, commands: Receiver<Command>, events: Sender<Event>) {
    if let Err(message) = run(request, commands, &events) {
        let _ = events.send(Event::Failed(message));
    }
}

fn run(
    request: Request,
    commands: Receiver<Command>,
    events: &Sender<Event>,
) -> Result<(), String> {
    gst::init().map_err(|_| "Could not initialize GStreamer for recording.".to_owned())?;
    let pipeline = build_pipeline(&request)?;
    let bus = pipeline
        .bus()
        .ok_or_else(|| "Could not monitor the recording pipeline.".to_owned())?;
    pipeline.set_state(gst::State::Playing).map_err(|_| {
        "Could not start recording. Check the camera and GStreamer plugins.".to_owned()
    })?;

    let mut announced = false;
    let mut stopping_since = None;
    loop {
        match commands.try_recv() {
            Ok(Command::Stop) | Err(TryRecvError::Disconnected) if stopping_since.is_none() => {
                // EOS lets matroskamux write its indexes before the file is closed.
                if !pipeline.send_event(gst::event::Eos::new()) {
                    let _ = pipeline.set_state(gst::State::Null);
                    return Err("Could not finalize the recording.".into());
                }
                stopping_since = Some(Instant::now());
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) | Ok(_) => {}
        }
        if stopping_since.is_some_and(|started| started.elapsed() > STOP_TIMEOUT) {
            let _ = pipeline.set_state(gst::State::Null);
            return Err("Timed out while finalizing the recording.".into());
        }
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            match message.view() {
                gst::MessageView::Error(error) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    return Err(format!("Recording failed: {}", error.error()));
                }
                gst::MessageView::Eos(..) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    let _ = events.send(Event::Finished);
                    return Ok(());
                }
                gst::MessageView::StateChanged(state)
                    if state
                        .src()
                        .is_some_and(|source| source == pipeline.upcast_ref::<gst::Object>())
                        && state.current() == gst::State::Playing
                        && !announced =>
                {
                    announced = true;
                    let _ = events.send(Event::Started);
                }
                _ => {}
            }
        }
    }
}

fn build_pipeline(request: &Request) -> Result<gst::Pipeline, String> {
    // Create branches only for streams the camera actually advertises.
    // Pre-requesting muxer pads for absent streams prevents muxing and preroll.
    let description = format!(
        "matroskamux name=mux ! filesink name=sink \
         rtspsrc name=source latency={RTSP_LATENCY_MS} drop-on-latency=true protocols=tcp"
    );
    let pipeline = gst::parse::launch(&description)
        .map_err(|_| {
            "Could not build the recording pipeline. Required GStreamer plugins may be missing."
                .to_owned()
        })?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Could not build the recording pipeline.".to_owned())?;
    let source = pipeline
        .by_name("source")
        .ok_or_else(|| "Could not configure the recording source.".to_owned())?;
    source.set_property("location", &request.uri);
    source.set_property("user-id", &request.username);
    source.set_property("user-pw", &request.password);
    let mux = pipeline
        .by_name("mux")
        .ok_or_else(|| "Could not configure the recording muxer.".to_owned())?;
    let weak_pipeline = pipeline.downgrade();
    let weak_mux = mux.downgrade();
    // Hold the first packets until all advertised streams have requested muxer
    // pads; otherwise Matroska may write its header before a later track exists.
    let pending = Arc::new(Mutex::new((
        false,
        Vec::<(gst::Pad, gst::PadProbeId)>::new(),
    )));
    let pads_ready = Arc::clone(&pending);
    source.connect_no_more_pads(move |_| {
        let mut pending = pads_ready.lock().unwrap();
        pending.0 = true;
        for (pad, probe) in pending.1.drain(..) {
            pad.remove_probe(probe);
        }
    });
    source.connect_pad_added(move |source, pad| {
        let Some(caps) = pad.current_caps() else {
            return;
        };
        let Some(structure) = caps.structure(0) else {
            return;
        };
        let media = structure.get::<&str>("media").unwrap_or("");
        let encoding = structure.get::<&str>("encoding-name").unwrap_or("");
        let branch = match (media, encoding) {
            ("video", "H264") => "rtph264depay ! h264parse config-interval=-1",
            ("video", "H265") => "rtph265depay ! h265parse config-interval=-1",
            ("audio", "MPEG4-GENERIC") => "rtpmp4gdepay ! aacparse",
            ("audio", "OPUS") => "rtpopusdepay ! opusparse",
            ("audio", "PCMA") => "rtppcmadepay",
            ("audio", "PCMU") => "rtppcmudepay",
            _ => return,
        };
        let mut pending = pending.lock().unwrap();
        if !pending.0
            && let Some(probe) = pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
                gst::PadProbeReturn::Ok
            })
        {
            pending.1.push((pad.clone(), probe));
        }
        let result = weak_pipeline
            .upgrade()
            .zip(weak_mux.upgrade())
            .ok_or(())
            .and_then(|(pipeline, mux)| {
                link_recording_stream(&pipeline, &mux, pad, branch).map_err(|_| ())
            });
        drop(pending);
        if result.is_err() {
            gst::element_error!(
                source,
                gst::StreamError::Failed,
                ("Could not configure a recording stream. Check the GStreamer plugins.")
            );
        }
    });
    let destination = request.destination.to_string_lossy();
    let sink = pipeline
        .by_name("sink")
        .ok_or_else(|| "Could not configure the recording destination.".to_owned())?;
    sink.set_property("location", destination.as_ref());
    Ok(pipeline)
}

fn link_recording_stream(
    pipeline: &gst::Pipeline,
    mux: &gst::Element,
    pad: &gst::Pad,
    description: &str,
) -> Result<(), gst::glib::BoolError> {
    let bin = gst::parse::bin_from_description(description, true)
        .map_err(|_| gst::glib::bool_error!("Could not create recording stream"))?;
    pipeline.add(&bin)?;
    bin.link(mux)?;
    bin.sync_state_with_parent()?;
    let sink = bin
        .static_pad("sink")
        .ok_or_else(|| gst::glib::bool_error!("Missing recording sink pad"))?;
    pad.link(&sink)
        .map_err(|_| gst::glib::bool_error!("Could not link recording stream"))?;
    Ok(())
}

/// Validates the destination before the worker starts, producing an actionable
/// storage error instead of an opaque pipeline failure.
pub fn validate_destination(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "The selected recording location has no folder.".to_owned())?;
    if !parent.is_dir() {
        return Err("The selected recording folder is not available.".into());
    }
    if path.exists() {
        return Err("A file already exists at the selected recording location.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_finalizes_a_single_audio_stream() {
        gst::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mkv");
        let pipeline = build_pipeline(&Request {
            uri: "rtsp://127.0.0.1/test".into(),
            username: String::new(),
            password: String::new(),
            destination: path.clone(),
        })
        .unwrap();
        pipeline
            .remove(&pipeline.by_name("source").unwrap())
            .unwrap();
        let source = gst::parse::bin_from_description(
            "audiotestsrc num-buffers=50 ! audioconvert ! alawenc ! rtppcmapay",
            true,
        )
        .unwrap();
        pipeline.add(&source).unwrap();
        link_recording_stream(
            &pipeline,
            &pipeline.by_name("mux").unwrap(),
            &source.static_pad("src").unwrap(),
            "rtppcmadepay",
        )
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut started = false;
        let mut finished = false;
        while Instant::now() < deadline {
            if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                match message.view() {
                    gst::MessageView::StateChanged(state)
                        if state
                            .src()
                            .is_some_and(|s| s == pipeline.upcast_ref::<gst::Object>())
                            && state.current() == gst::State::Playing =>
                    {
                        started = true
                    }
                    gst::MessageView::Eos(..) => {
                        finished = true;
                        break;
                    }
                    gst::MessageView::Error(error) => panic!("Recording failed: {}", error.error()),
                    _ => {}
                }
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(started, "single-stream pipeline never started");
        assert!(finished, "single-stream pipeline never finalized");
        assert!(std::fs::metadata(&path).unwrap().len() > 1024);
        let decoder = gst::parse::launch("filesrc name=input ! matroskademux ! alawdec ! fakesink")
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        decoder
            .by_name("input")
            .unwrap()
            .set_property("location", path.to_str().unwrap());
        decoder.set_state(gst::State::Playing).unwrap();
        let message = decoder
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("recording playback timed out");
        decoder.set_state(gst::State::Null).unwrap();
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "{message:?}"
        );
    }
}
