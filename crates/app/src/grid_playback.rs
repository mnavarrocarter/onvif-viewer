use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use adw::prelude::*;
use adw::{glib, gtk};
use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::watch;

use crate::storage::Source;

const RTSP_LATENCY_MS: u32 = 500;

pub(crate) fn lowest_resolution_profile(profiles: &[onvif::Profile]) -> Option<&onvif::Profile> {
    profiles
        .iter()
        .filter_map(|profile| match (profile.width, profile.height) {
            (Some(width), Some(height)) if width > 0 && height > 0 => {
                Some((u64::from(width) * u64::from(height), profile))
            }
            _ => None,
        })
        .min_by_key(|(area, _)| *area)
        .map(|(_, profile)| profile)
        .or_else(|| profiles.first())
}

enum Response {
    Stream(String),
    Failed(String),
}

struct State {
    playing: bool,
    muted: bool,
    volume: f64,
    cancel: Option<watch::Sender<bool>>,
    responses: Option<Receiver<Response>>,
    player: Option<gst::Element>,
    poll_source: Option<glib::SourceId>,
    stream_uri: Option<String>,
    snapshot_response: Option<Receiver<Result<Vec<u8>, &'static str>>>,
    snapshot_dialog: Option<gtk::FileChooserNative>,
    snapshot_save: Option<gtk::gio::Cancellable>,
    snapshot_generation: u64,
    recording: Option<recording::Recording>,
    recording_dialog: Option<gtk::FileChooserNative>,
    recording_starting: bool,
    recording_stopping: bool,
    closed: bool,
}

pub struct GridPlayback {
    source: RefCell<Source>,
    widget: gtk::Overlay,
    picture: gtk::Picture,
    status: gtk::Label,
    snapshot_button: gtk::Button,
    record_button: gtk::Button,
    capture_polling: Cell<bool>,
    state: RefCell<State>,
}

impl GridPlayback {
    pub fn new(source: Source) -> Rc<Self> {
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_keep_aspect_ratio(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);

        let status = gtk::Label::new(Some("Stopped"));
        status.set_wrap(true);
        status.set_halign(gtk::Align::Center);
        status.set_valign(gtk::Align::Center);
        status.add_css_class("grid-status");

        let widget = gtk::Overlay::new();
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        // Keep the paintable's intrinsic resolution out of the grid's size request.
        // The picture is allocated to the overlay instead of measuring the tile.
        widget.set_child(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
        widget.add_overlay(&picture);
        widget.add_overlay(&status);

        let snapshot_button = gtk::Button::from_icon_name("camera-photo-symbolic");
        snapshot_button.set_tooltip_text(Some("Take Snapshot"));
        snapshot_button.add_css_class("flat");
        snapshot_button.add_css_class("circular");
        snapshot_button.set_sensitive(false);
        let record_button = gtk::Button::from_icon_name("media-record-symbolic");
        record_button.set_tooltip_text(Some("Start Recording"));
        record_button.add_css_class("flat");
        record_button.add_css_class("circular");
        record_button.set_sensitive(false);
        let playback = Rc::new(Self {
            source: RefCell::new(source),
            widget,
            picture,
            status,
            snapshot_button,
            record_button,
            capture_polling: Cell::new(false),
            state: RefCell::new(State {
                playing: false,
                muted: true,
                volume: 1.0,
                cancel: None,
                responses: None,
                player: None,
                poll_source: None,
                stream_uri: None,
                snapshot_response: None,
                snapshot_dialog: None,
                snapshot_save: None,
                snapshot_generation: 0,
                recording: None,
                recording_dialog: None,
                recording_starting: false,
                recording_stopping: false,
                closed: false,
            }),
        });
        let weak = Rc::downgrade(&playback);
        playback.snapshot_button.connect_clicked(move |_| {
            if let Some(playback) = weak.upgrade() {
                playback.capture();
            }
        });
        let weak = Rc::downgrade(&playback);
        playback.record_button.connect_clicked(move |_| {
            if let Some(playback) = weak.upgrade() {
                playback.choose_recording();
            }
        });
        playback
    }

    pub fn widget(&self) -> gtk::Widget {
        self.widget.clone().upcast()
    }

    pub fn snapshot_button(&self) -> &gtk::Button {
        &self.snapshot_button
    }

    pub fn record_button(&self) -> &gtk::Button {
        &self.record_button
    }

    fn window(&self) -> Option<gtk::Window> {
        self.widget.root()?.downcast::<gtk::Window>().ok()
    }

    fn error(&self, title: &str, message: &str) {
        if let Some(window) = self.window() {
            let dialog = adw::MessageDialog::new(Some(&window), Some(title), Some(message));
            dialog.add_response("close", "Close");
            dialog.present();
        }
    }

    fn update_capture_buttons(&self) {
        let state = self.state.borrow();
        self.snapshot_button.set_sensitive(
            !state.closed
                && state.player.is_some()
                && state.snapshot_response.is_none()
                && state.snapshot_dialog.is_none()
                && state.snapshot_save.is_none(),
        );
        let (icon, tooltip, enabled) = if state.recording_stopping {
            ("media-playback-stop-symbolic", "Saving Recording…", false)
        } else if state.recording_starting {
            ("media-record-symbolic", "Starting Recording…", false)
        } else if state.recording.is_some() {
            ("media-playback-stop-symbolic", "Stop Recording", true)
        } else {
            (
                "media-record-symbolic",
                "Start Recording",
                state.stream_uri.is_some(),
            )
        };
        self.record_button.set_icon_name(icon);
        self.record_button.set_tooltip_text(Some(tooltip));
        if state.recording.is_some() {
            self.record_button.add_css_class("recording");
        } else {
            self.record_button.remove_css_class("recording");
        }
        self.record_button
            .set_sensitive(enabled && !state.closed && state.recording_dialog.is_none());
    }

    fn capture(self: &Rc<Self>) {
        let player = {
            let state = self.state.borrow();
            if state.closed
                || state.snapshot_response.is_some()
                || state.snapshot_dialog.is_some()
                || state.snapshot_save.is_some()
            {
                return;
            }
            let Some(player) = state.player.clone() else {
                return;
            };
            player
        };
        let (tx, rx) = mpsc::channel();
        if std::thread::Builder::new()
            .name("grid-snapshot-encoder".into())
            .spawn(move || {
                let _ = tx.send(crate::feed::capture_frame(&player));
            })
            .is_err()
        {
            self.error(
                "Could not capture snapshot",
                "Could not start the snapshot worker.",
            );
            return;
        }
        self.state.borrow_mut().snapshot_response = Some(rx);
        self.ensure_capture_poll();
        self.update_capture_buttons();
    }

    fn save_snapshot(self: &Rc<Self>, bytes: Vec<u8>) {
        let Some(window) = self.window() else { return };
        let dialog = gtk::FileChooserNative::new(
            Some("Save Snapshot"),
            Some(&window),
            gtk::FileChooserAction::Save,
            Some("Save"),
            Some("Cancel"),
        );
        dialog.set_modal(true);
        dialog.set_current_name(&crate::feed::timestamped_filename(
            &self.source.borrow().name,
            "snapshot",
            "jpg",
        ));
        let generation = self.state.borrow().snapshot_generation;
        let bytes = RefCell::new(Some(bytes));
        let weak = Rc::downgrade(self);
        dialog.connect_response(move |dialog, response| {
            let Some(playback) = weak.upgrade() else {
                return;
            };
            {
                let mut state = playback.state.borrow_mut();
                if state.closed || state.snapshot_generation != generation {
                    return;
                }
                state.snapshot_dialog = None;
            }
            if response == gtk::ResponseType::Accept {
                if let (Some(file), Some(bytes)) = (dialog.file(), bytes.borrow_mut().take()) {
                    let cancel = gtk::gio::Cancellable::new();
                    playback.state.borrow_mut().snapshot_save = Some(cancel.clone());
                    let weak = Rc::downgrade(&playback);
                    file.replace_contents_async(
                        bytes,
                        None,
                        false,
                        gtk::gio::FileCreateFlags::NONE,
                        Some(&cancel),
                        move |result| {
                            let Some(playback) = weak.upgrade() else {
                                return;
                            };
                            {
                                let mut state = playback.state.borrow_mut();
                                if state.closed || state.snapshot_generation != generation {
                                    return;
                                }
                                state.snapshot_save = None;
                            }
                            playback.update_capture_buttons();
                            if let Err((_, error)) = result {
                                playback.error("Could not save snapshot", &error.to_string());
                            }
                        },
                    );
                    return;
                }
                playback.error("Could not save snapshot", "No destination was selected.");
            }
            playback.update_capture_buttons();
        });
        self.state.borrow_mut().snapshot_dialog = Some(dialog.clone());
        self.update_capture_buttons();
        dialog.show();
    }

    fn choose_recording(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            if state.closed || state.recording_starting || state.recording_stopping {
                return;
            }
            if let Some(recorder) = &state.recording {
                recorder.stop();
                state.recording_stopping = true;
                drop(state);
                self.update_capture_buttons();
                return;
            }
            if state.stream_uri.is_none() || state.recording_dialog.is_some() {
                return;
            }
        }
        let Some(window) = self.window() else { return };
        let dialog = gtk::FileChooserNative::new(
            Some("Save Recording"),
            Some(&window),
            gtk::FileChooserAction::Save,
            Some("Start Recording"),
            Some("Cancel"),
        );
        dialog.set_modal(true);
        dialog.set_current_name(&crate::feed::timestamped_filename(
            &self.source.borrow().name,
            "recording",
            "mkv",
        ));
        let weak = Rc::downgrade(self);
        dialog.connect_response(move |dialog, response| {
            let Some(playback) = weak.upgrade() else {
                return;
            };
            playback.state.borrow_mut().recording_dialog = None;
            if response == gtk::ResponseType::Accept {
                if let Some(path) = dialog.file().and_then(|file| file.path()) {
                    playback.start_recording(path);
                } else {
                    playback.error(
                        "Could not record camera feed",
                        "No local recording location was selected.",
                    );
                }
            }
            playback.update_capture_buttons();
        });
        self.state.borrow_mut().recording_dialog = Some(dialog.clone());
        self.update_capture_buttons();
        dialog.show();
    }

    fn start_recording(self: &Rc<Self>, destination: std::path::PathBuf) {
        if let Err(error) = recording::validate_destination(&destination) {
            self.error("Could not record camera feed", &error);
            return;
        }
        let uri = {
            let state = self.state.borrow();
            if state.closed || state.recording.is_some() {
                return;
            }
            let Some(uri) = state.stream_uri.clone() else {
                drop(state);
                self.error(
                    "Could not record camera feed",
                    "The stream is no longer active.",
                );
                return;
            };
            uri
        };
        let source = self.source.borrow();
        let request = recording::Request {
            uri,
            username: source.username.clone(),
            password: source.password.clone(),
            destination,
        };
        drop(source);
        match recording::Recording::start(request) {
            Ok(recorder) => {
                let mut state = self.state.borrow_mut();
                state.recording = Some(recorder);
                state.recording_starting = true;
                drop(state);
                self.ensure_capture_poll();
            }
            Err(error) => self.error("Could not record camera feed", &error),
        }
        self.update_capture_buttons();
    }

    fn ensure_capture_poll(self: &Rc<Self>) {
        if self.capture_polling.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let Some(playback) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            playback.poll_capture();
            let state = playback.state.borrow();
            if state.snapshot_response.is_some() || state.recording.is_some() {
                glib::ControlFlow::Continue
            } else {
                playback.capture_polling.set(false);
                glib::ControlFlow::Break
            }
        });
    }

    fn poll_capture(self: &Rc<Self>) {
        let snapshot = self
            .state
            .borrow()
            .snapshot_response
            .as_ref()
            .map(Receiver::try_recv);
        match snapshot {
            Some(Ok(result)) => {
                self.state.borrow_mut().snapshot_response = None;
                match result {
                    Ok(bytes) => self.save_snapshot(bytes),
                    Err(error) => self.error("Could not capture snapshot", error),
                }
                self.update_capture_buttons();
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.state.borrow_mut().snapshot_response = None;
                self.update_capture_buttons();
                self.error(
                    "Could not capture snapshot",
                    "The snapshot worker ended unexpectedly.",
                );
            }
            _ => {}
        }
        let event = self
            .state
            .borrow()
            .recording
            .as_ref()
            .map(|recorder| recorder.try_event());
        match event {
            Some(Ok(recording::Event::Started)) => {
                self.state.borrow_mut().recording_starting = false;
                self.update_capture_buttons();
            }
            Some(Ok(recording::Event::Finished)) => {
                let mut state = self.state.borrow_mut();
                state.recording = None;
                state.recording_starting = false;
                state.recording_stopping = false;
                drop(state);
                self.update_capture_buttons();
            }
            Some(Ok(recording::Event::Failed(error))) => {
                let mut state = self.state.borrow_mut();
                state.recording = None;
                state.recording_starting = false;
                state.recording_stopping = false;
                drop(state);
                self.update_capture_buttons();
                self.error("Could not record camera feed", &error);
            }
            Some(Err(TryRecvError::Disconnected)) => {
                let mut state = self.state.borrow_mut();
                state.recording = None;
                state.recording_starting = false;
                state.recording_stopping = false;
                drop(state);
                self.update_capture_buttons();
                self.error(
                    "Could not record camera feed",
                    "The recording worker ended unexpectedly.",
                );
            }
            _ => {}
        }
    }

    pub fn close(&self) {
        let (snapshot_dialog, snapshot_save, recording_dialog, recorder) = {
            let mut state = self.state.borrow_mut();
            state.closed = true;
            state.snapshot_generation = state.snapshot_generation.wrapping_add(1);
            state.snapshot_response = None;
            (
                state.snapshot_dialog.take(),
                state.snapshot_save.take(),
                state.recording_dialog.take(),
                state.recording.take(),
            )
        };
        if let Some(dialog) = snapshot_dialog {
            dialog.hide();
        }
        if let Some(cancel) = snapshot_save {
            cancel.cancel();
        }
        if let Some(dialog) = recording_dialog {
            dialog.hide();
        }
        if let Some(recorder) = recorder {
            recorder.stop();
            let window = self.window().map(|window| window.downgrade());
            // Keep the application alive until the independent muxer has finalized.
            let hold = gtk::gio::Application::default().map(|app| app.hold());
            glib::timeout_add_local(Duration::from_millis(100), move || {
                let _hold = &hold;
                match recorder.try_event() {
                    Ok(recording::Event::Finished) => glib::ControlFlow::Break,
                    Ok(recording::Event::Failed(error)) => {
                        if let Some(window) = window
                            .as_ref()
                            .and_then(|window| window.upgrade())
                            .filter(|window| window.is_visible())
                        {
                            let dialog = adw::MessageDialog::new(
                                Some(&window),
                                Some("Could not finalize recording"),
                                Some(&error),
                            );
                            dialog.add_response("close", "Close");
                            dialog.present();
                        } else {
                            eprintln!("Could not finalize recording: {error}");
                        }
                        glib::ControlFlow::Break
                    }
                    Err(TryRecvError::Disconnected) => {
                        eprintln!("The recording worker ended unexpectedly while finalizing.");
                        glib::ControlFlow::Break
                    }
                    _ => glib::ControlFlow::Continue,
                }
            });
        }
        self.stop();
        self.update_capture_buttons();
    }

    pub fn play(self: &Rc<Self>) {
        if self.is_playing() {
            return;
        }
        // A failed connection may have left a poll waiting for its next tick.
        if let Some(source) = self.state.borrow_mut().poll_source.take() {
            source.remove();
        }
        self.state.borrow_mut().playing = true;
        if !self.connect_camera() {
            return;
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local(Duration::from_millis(100), move || {
            let Some(playback) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            playback.poll()
        });
        self.state.borrow_mut().poll_source = Some(source);
    }

    pub fn stop(&self) {
        if let Some(source) = self.state.borrow_mut().poll_source.take() {
            source.remove();
        }
        self.clear_playback();
        self.state.borrow_mut().playing = false;
        self.status.set_text("Stopped");
        self.status.set_visible(true);
    }

    pub fn is_playing(&self) -> bool {
        self.state.borrow().playing
    }

    pub fn set_muted(&self, muted: bool) {
        let mut state = self.state.borrow_mut();
        state.muted = muted;
        if let Some(player) = &state.player {
            player.set_property("mute", muted);
        }
    }

    pub fn set_volume(&self, volume: f64) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut state = self.state.borrow_mut();
        state.volume = volume;
        if let Some(player) = &state.player {
            player.set_property("volume", volume);
        }
    }

    pub fn update_source(&self, source: Source) {
        let reconnect = {
            let current = self.source.borrow();
            let current_profile = lowest_resolution_profile(&current.profiles)
                .map(|profile| (&profile.token, profile.width, profile.height));
            let updated_profile = lowest_resolution_profile(&source.profiles)
                .map(|profile| (&profile.token, profile.width, profile.height));
            current.address != source.address
                || current.username != source.username
                || current.password != source.password
                || current_profile != updated_profile
        };
        *self.source.borrow_mut() = source;
        if reconnect && self.is_playing() {
            self.clear_playback();
            self.connect_camera();
        }
    }

    fn clear_playback(&self) {
        let mut state = self.state.borrow_mut();
        if let Some(cancel) = state.cancel.take() {
            let _ = cancel.send(true);
        }
        state.responses = None;
        state.stream_uri = None;
        if let Some(player) = state.player.take() {
            // RTSP teardown can block; do not stall the GTK main loop.
            std::thread::spawn(move || {
                let _ = player.set_state(gst::State::Null);
            });
        }
        drop(state);
        self.picture.set_paintable(gtk::gdk::Paintable::NONE);
        self.update_capture_buttons();
    }

    fn fail(&self, message: &str) {
        self.clear_playback();
        self.state.borrow_mut().playing = false;
        self.status.set_text(message);
        self.status.set_visible(true);
    }

    fn connect_camera(&self) -> bool {
        let source = self.source.borrow();
        let address = source.address.clone();
        let username = source.username.clone();
        let password = source.password.clone();
        drop(source);

        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let (response_tx, response_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("onvif-grid-camera".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let _ = response_tx.send(Response::Failed(
                            "Could not initialize the camera connection.".into(),
                        ));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let camera = match tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => return,
                        result = onvif::connect(&address, &username, &password) => result,
                    } {
                        Ok(camera) => camera,
                        Err(error) => {
                            let _ = response_tx.send(Response::Failed(error.to_string()));
                            return;
                        }
                    };
                    let Some(profile) = lowest_resolution_profile(&camera.profiles) else {
                        let _ = response_tx
                            .send(Response::Failed("The camera has no media profiles.".into()));
                        return;
                    };
                    let result = tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => return,
                        result = camera.stream_uri(&profile.token) => result,
                    };
                    let response = match result {
                        Ok(uri) => Response::Stream(uri),
                        Err(error) => Response::Failed(error.to_string()),
                    };
                    let _ = response_tx.send(response);
                });
            });
        if worker.is_err() {
            self.fail("Could not start the camera connection worker.");
            return false;
        }
        let mut state = self.state.borrow_mut();
        state.cancel = Some(cancel_tx);
        state.responses = Some(response_rx);
        drop(state);
        self.status.set_text("Connecting to camera…");
        self.status.set_visible(true);
        true
    }

    fn start_player(&self, uri: &str) -> Result<(), &'static str> {
        gst::init().map_err(|_| "Could not initialize GStreamer.")?;
        let source = self.source.borrow();
        let username = source.username.clone();
        let password = source.password.clone();
        drop(source);
        let mut uri =
            url::Url::parse(uri).map_err(|_| "The camera returned an invalid stream URI.")?;
        if !uri.username().is_empty() || uri.password().is_some() {
            uri.set_password(None)
                .map_err(|_| "The camera returned an invalid stream URI.")?;
            uri.set_username("")
                .map_err(|_| "The camera returned an invalid stream URI.")?;
        }
        let sink = gst::ElementFactory::make("gtk4paintablesink")
            .build()
            .map_err(|_| "The GStreamer GTK4 video sink is not installed.")?;
        let paintable = sink.property::<gtk::gdk::Paintable>("paintable");
        let state = self.state.borrow();
        let player = gst::ElementFactory::make("playbin")
            .property("video-sink", &sink)
            .property("uri", uri.as_str())
            .property("volume", state.volume)
            .property("mute", state.muted)
            .build()
            .map_err(|_| "The GStreamer playback plugin is not installed.")?;
        drop(state);
        player.connect("source-setup", false, move |values| {
            if let Ok(source) = values[1].get::<gst::Element>() {
                if source
                    .factory()
                    .is_some_and(|factory| factory.name() == "rtspsrc")
                {
                    source.set_property("latency", RTSP_LATENCY_MS);
                    source.set_property("drop-on-latency", true);
                }
                if source.find_property("user-id").is_some()
                    && source.find_property("user-pw").is_some()
                {
                    source.set_property("user-id", &username);
                    source.set_property("user-pw", &password);
                }
            }
            None
        });
        self.picture.set_paintable(Some(&paintable));
        if player.set_state(gst::State::Playing).is_err() {
            std::thread::spawn(move || {
                let _ = player.set_state(gst::State::Null);
            });
            self.picture.set_paintable(gtk::gdk::Paintable::NONE);
            return Err("Could not start the stream. Check the camera and GStreamer plugins.");
        }
        self.state.borrow_mut().player = Some(player);
        self.state.borrow_mut().stream_uri = Some(uri.to_string());
        self.update_capture_buttons();
        Ok(())
    }

    fn poll(&self) -> glib::ControlFlow {
        if !self.is_playing() {
            self.state.borrow_mut().poll_source = None;
            return glib::ControlFlow::Break;
        }
        let response = {
            let state = self.state.borrow();
            state.responses.as_ref().map(Receiver::try_recv)
        };
        match response {
            Some(Ok(response)) => {
                let mut state = self.state.borrow_mut();
                state.responses = None;
                state.cancel = None;
                drop(state);
                match response {
                    Response::Stream(uri) => match self.start_player(&uri) {
                        Ok(()) => self.status.set_visible(false),
                        Err(message) => self.fail(message),
                    },
                    Response::Failed(message) => self.fail(&message),
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.fail("Camera connection ended unexpectedly.");
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        let bus = self
            .state
            .borrow()
            .player
            .as_ref()
            .and_then(|player| player.bus());
        if let Some(bus) = bus {
            while let Some(message) = bus.pop() {
                if matches!(
                    message.view(),
                    gst::MessageView::Error(_) | gst::MessageView::Eos(_)
                ) {
                    self.fail("The stream ended or could not be played. Check the camera and GStreamer plugins.");
                    break;
                }
            }
        }
        if self.is_playing() {
            glib::ControlFlow::Continue
        } else {
            self.state.borrow_mut().poll_source = None;
            glib::ControlFlow::Break
        }
    }
}

impl Drop for GridPlayback {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::lowest_resolution_profile;

    fn profile(token: &str, width: Option<u32>, height: Option<u32>) -> onvif::Profile {
        onvif::Profile {
            token: token.into(),
            name: token.into(),
            width,
            height,
            codec: None,
            audio_codec: None,
        }
    }

    #[test]
    fn chooses_the_smallest_pixel_area() {
        let profiles = [
            profile("large", Some(1920), Some(1080)),
            profile("small", Some(640), Some(480)),
            profile("narrow", Some(320), Some(1080)),
        ];
        assert_eq!(lowest_resolution_profile(&profiles).unwrap().token, "small");
    }

    #[test]
    fn ignores_missing_and_zero_dimensions_when_known_sizes_exist() {
        let profiles = [
            profile("missing", None, Some(1)),
            profile("zero", Some(0), Some(1)),
            profile("valid", Some(1280), Some(720)),
        ];
        assert_eq!(lowest_resolution_profile(&profiles).unwrap().token, "valid");
    }

    #[test]
    fn falls_back_to_first_when_no_resolution_is_known() {
        let profiles = [
            profile("first", None, None),
            profile("zero", Some(0), Some(720)),
            profile("missing", Some(640), None),
        ];
        assert_eq!(lowest_resolution_profile(&profiles).unwrap().token, "first");
        assert!(lowest_resolution_profile(&[]).is_none());
    }
}
