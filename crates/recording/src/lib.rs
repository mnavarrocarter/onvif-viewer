//! UI-independent local RTSP recording.
//!
//! Recordings are remuxed into Matroska instead of decoded and re-encoded.
//! This keeps supported camera video and audio compressed and makes this
//! component usable by a future background recorder as well as a visible feed.

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
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
    let location = gst::glib::shell_quote(request.destination.as_os_str())
        .to_string_lossy()
        .into_owned();
    // Every branch stays compressed. Matroska handles the common ONVIF RTSP
    // combinations below, including AAC, Opus and G.711 audio when present.
    let description = format!(
        "matroskamux name=mux ! filesink location={location} \
         rtspsrc name=source latency={RTSP_LATENCY_MS} drop-on-latency=true protocols=tcp \
         source. ! application/x-rtp,media=video,encoding-name=H264 ! rtph264depay ! h264parse config-interval=-1 ! mux. \
         source. ! application/x-rtp,media=video,encoding-name=H265 ! rtph265depay ! h265parse config-interval=-1 ! mux. \
         source. ! application/x-rtp,media=audio,encoding-name=MPEG4-GENERIC ! rtpmp4gdepay ! aacparse ! mux. \
         source. ! application/x-rtp,media=audio,encoding-name=OPUS ! rtpopusdepay ! opusparse ! mux. \
         source. ! application/x-rtp,media=audio,encoding-name=PCMA ! rtppcmadepay ! mux. \
         source. ! application/x-rtp,media=audio,encoding-name=PCMU ! rtppcmudepay ! mux."
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
    Ok(pipeline)
}

/// Suggest a Matroska filename for a camera.
pub fn default_filename(camera_name: &str) -> String {
    let stem: String = camera_name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "{}-recording.mkv",
        if stem.is_empty() { "camera" } else { &stem }
    )
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
    use super::default_filename;

    #[test]
    fn filename_is_visible_and_safe() {
        assert_eq!(
            default_filename("Front / Door"),
            "Front---Door-recording.mkv"
        );
        assert_eq!(default_filename(""), "camera-recording.mkv");
    }
}
