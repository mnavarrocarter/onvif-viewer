use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::{glib, gtk};
use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::watch;

use crate::ptz::PtzControl;
use crate::storage::Source;

// Keep the RTSP jitter buffer bounded without forcing sinks to run unsynchronized.
const RTSP_LATENCY_MS: u32 = 500;
const DROP_INDICATOR_DURATION: Duration = Duration::from_secs(5);

enum Response {
    Stream { uri: String, profile: String },
    Failed(String),
}

#[derive(Default)]
struct State {
    cancel: Option<watch::Sender<bool>>,
    responses: Option<Receiver<Response>>,
    snapshot_response: Option<Receiver<Result<Vec<u8>, &'static str>>>,
    snapshot_dialog: Option<gtk::FileChooserNative>,
    snapshot_save: Option<gtk::gio::Cancellable>,
    snapshot_generation: u64,
    recording: Option<recording::Recording>,
    recording_dialog: Option<gtk::FileChooserNative>,
    recording_starting: bool,
    recording_stopping: bool,
    stream_uri: Option<String>,
    closed: bool,
    player: Option<gst::Element>,
    last_drop: Option<Instant>,
    dropped_packets: u64,
}

struct Ui {
    source: RefCell<Source>,
    title: adw::WindowTitle,
    status: gtk::Label,
    drop_indicator: gtk::Label,
    picture: gtk::Picture,
    window: glib::WeakRef<adw::Window>,
    snapshot_button: gtk::Button,
    record_button: gtk::Button,
    ptz: Rc<PtzControl>,
    mute: gtk::ToggleButton,
    volume: gtk::Scale,
    choices: gtk::Box,
    profile_popover: gtk::Popover,
    on_select: Rc<dyn Fn(String)>,
    state: RefCell<State>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<(glib::WeakRef<adw::Window>, std::rc::Weak<Ui>)>> =
        const { RefCell::new(Vec::new()) };
}

fn stop_player(state: &mut State) {
    if let Some(player) = state.player.take() {
        state.last_drop = None;
        state.dropped_packets = 0;
        std::thread::spawn(move || {
            let _ = player.set_state(gst::State::Null);
        });
    }
}

pub(crate) fn capture_frame(player: &gst::Element) -> Result<Vec<u8>, &'static str> {
    let caps = gst::Caps::builder("image/jpeg").build();
    let sample = player
        .emit_by_name::<Option<gst::Sample>>("convert-sample", &[&caps])
        .ok_or("No video frame is available, or the GStreamer JPEG encoder is missing.")?;
    let buffer = sample
        .buffer()
        .ok_or("The captured video frame is empty.")?;
    let map = buffer
        .map_readable()
        .map_err(|_| "Could not read the captured video frame.")?;
    Ok(map.as_slice().to_vec())
}

fn suggested_filename(name: &str, kind: &str, extension: &str, timestamp: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "{}-{kind}-{timestamp}.{extension}",
        if stem.is_empty() { "camera" } else { &stem }
    )
}

pub(crate) fn timestamped_filename(name: &str, kind: &str, extension: &str) -> String {
    let timestamp = chrono::Local::now()
        .format("%Y-%m-%d_%H-%M-%S%.3f")
        .to_string();
    suggested_filename(name, kind, extension, &timestamp)
}

impl Ui {
    fn recording_error(&self, message: &str) {
        if let Some(window) = self.window.upgrade() {
            let dialog = adw::MessageDialog::new(
                Some(&window),
                Some("Could not record camera feed"),
                Some(message),
            );
            dialog.add_response("close", "Close");
            dialog.present();
        }
    }

    fn update_record_button(&self) {
        let state = self.state.borrow();
        let (icon, label, sensitive) = if state.recording_starting {
            ("media-record-symbolic", "Starting recording", false)
        } else if state.recording_stopping {
            ("media-record-symbolic", "Saving recording", false)
        } else if state.recording.is_some() {
            ("media-playback-stop-symbolic", "Stop recording", true)
        } else {
            (
                "media-record-symbolic",
                "Record clip",
                state.stream_uri.is_some() && !state.closed,
            )
        };
        self.record_button.set_icon_name(icon);
        self.record_button.set_tooltip_text(Some(label));
        self.record_button.set_sensitive(sensitive);
        if state.recording.is_some() {
            self.record_button.add_css_class("recording");
        } else {
            self.record_button.remove_css_class("recording");
        }
    }

    fn update_profiles(&self) {
        while let Some(child) = self.choices.first_child() {
            self.choices.remove(&child);
        }
        let source = self.source.borrow();
        let selected = source
            .selected_profile
            .as_deref()
            .and_then(|token| {
                source
                    .profiles
                    .iter()
                    .find(|profile| profile.token == token)
            })
            .or_else(|| {
                source.profiles.iter().find(|profile| {
                    profile.codec.is_some() || profile.width.is_some() || profile.height.is_some()
                })
            })
            .or_else(|| source.profiles.first());
        for profile in &source.profiles {
            let is_selected = selected.is_some_and(|selected| selected.token == profile.token);
            let label = format!(
                "{}  {}",
                if is_selected { "✓" } else { " " },
                Source::profile_label(profile)
            );
            let choice = gtk::Button::with_label(&label);
            choice.add_css_class("flat");
            let token = profile.token.clone();
            let on_select = Rc::clone(&self.on_select);
            let popover = self.profile_popover.downgrade();
            choice.connect_clicked(move |_| {
                if let Some(popover) = popover.upgrade() {
                    popover.popdown();
                }
                if !is_selected {
                    on_select(token.clone());
                }
            });
            self.choices.append(&choice);
        }
        if source.profiles.is_empty() {
            self.choices
                .append(&gtk::Label::new(Some("No profiles available")));
        }
    }

    fn start_recording(&self, destination: std::path::PathBuf) {
        if let Err(error) = recording::validate_destination(&destination) {
            self.recording_error(&error);
            return;
        }
        let uri = {
            let state = self.state.borrow();
            if state.closed || state.recording.is_some() {
                return;
            }
            let Some(uri) = state.stream_uri.clone() else {
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
                self.update_record_button();
            }
            Err(error) => self.recording_error(&error),
        }
    }

    fn choose_recording_destination(self: &Rc<Self>) {
        if self.state.borrow().recording.is_some() {
            self.stop_recording();
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = gtk::FileChooserNative::new(
            Some("Save Recording"),
            Some(&window),
            gtk::FileChooserAction::Save,
            Some("Start Recording"),
            Some("Cancel"),
        );
        dialog.set_modal(true);
        dialog.set_current_name(&timestamped_filename(
            &self.source.borrow().name,
            "recording",
            "mkv",
        ));
        let weak = Rc::downgrade(self);
        dialog.connect_response(move |dialog, response| {
            let Some(ui) = weak.upgrade() else { return };
            ui.state.borrow_mut().recording_dialog = None;
            if response == gtk::ResponseType::Accept {
                if let Some(path) = dialog.file().and_then(|file| file.path()) {
                    ui.start_recording(path);
                } else {
                    ui.recording_error("No local recording location was selected.");
                }
            }
            ui.update_record_button();
        });
        self.state.borrow_mut().recording_dialog = Some(dialog.clone());
        self.record_button.set_sensitive(false);
        dialog.show();
    }

    fn stop_recording(&self) {
        let mut state = self.state.borrow_mut();
        if let Some(recorder) = &state.recording {
            recorder.stop();
            state.recording_stopping = true;
        }
        drop(state);
        self.update_record_button();
    }

    fn cancel_snapshot(&self) {
        let (dialog, save) = {
            let mut state = self.state.borrow_mut();
            state.snapshot_generation = state.snapshot_generation.wrapping_add(1);
            state.snapshot_response = None;
            (state.snapshot_dialog.take(), state.snapshot_save.take())
        };
        if let Some(dialog) = dialog {
            dialog.hide();
        }
        if let Some(save) = save {
            save.cancel();
        }
        self.snapshot_button.set_sensitive(false);
    }

    fn snapshot_error(&self, title: &str, message: &str) {
        if let Some(window) = self.window.upgrade() {
            let dialog = adw::MessageDialog::new(Some(&window), Some(title), Some(message));
            dialog.add_response("close", "Close");
            dialog.present();
        }
    }

    fn update_snapshot_button(&self) {
        let state = self.state.borrow();
        self.snapshot_button.set_sensitive(
            !state.closed
                && state.player.is_some()
                && state.snapshot_response.is_none()
                && state.snapshot_dialog.is_none()
                && state.snapshot_save.is_none(),
        );
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
        let (response_tx, response_rx) = mpsc::channel();
        if std::thread::Builder::new()
            .name("snapshot-encoder".into())
            .spawn(move || {
                let _ = response_tx.send(capture_frame(&player));
            })
            .is_err()
        {
            self.snapshot_error(
                "Could not capture snapshot",
                "Could not start the snapshot worker.",
            );
            return;
        }
        self.state.borrow_mut().snapshot_response = Some(response_rx);
        self.update_snapshot_button();
    }

    fn save_snapshot(self: &Rc<Self>, bytes: Vec<u8>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = gtk::FileChooserNative::new(
            Some("Save Snapshot"),
            Some(&window),
            gtk::FileChooserAction::Save,
            Some("Save"),
            Some("Cancel"),
        );
        dialog.set_modal(true);
        dialog.set_current_name(&timestamped_filename(
            &self.source.borrow().name,
            "snapshot",
            "jpg",
        ));
        let generation = self.state.borrow().snapshot_generation;
        let bytes = RefCell::new(Some(bytes));
        let weak = Rc::downgrade(self);
        dialog.connect_response(move |dialog, response| {
            let Some(ui) = weak.upgrade() else { return };
            {
                let mut state = ui.state.borrow_mut();
                if state.closed || state.snapshot_generation != generation {
                    return;
                }
                state.snapshot_dialog = None;
            }
            if response == gtk::ResponseType::Accept {
                if let (Some(file), Some(bytes)) = (dialog.file(), bytes.borrow_mut().take()) {
                    let cancel = gtk::gio::Cancellable::new();
                    ui.state.borrow_mut().snapshot_save = Some(cancel.clone());
                    let weak = Rc::downgrade(&ui);
                    file.replace_contents_async(
                        bytes,
                        None,
                        false,
                        gtk::gio::FileCreateFlags::NONE,
                        Some(&cancel),
                        move |result| {
                            let Some(ui) = weak.upgrade() else { return };
                            {
                                let mut state = ui.state.borrow_mut();
                                if state.closed || state.snapshot_generation != generation {
                                    return;
                                }
                                state.snapshot_save = None;
                            }
                            ui.update_snapshot_button();
                            if let Err((_, error)) = result {
                                ui.snapshot_error("Could not save snapshot", &error.to_string());
                            }
                        },
                    );
                    return;
                }
                ui.snapshot_error("Could not save snapshot", "No destination was selected.");
            }
            ui.update_snapshot_button();
        });
        self.state.borrow_mut().snapshot_dialog = Some(dialog.clone());
        dialog.show();
    }

    fn close(&self) {
        self.ptz.shutdown();
        self.cancel_snapshot();
        let mut state = self.state.borrow_mut();
        state.closed = true;
        if let Some(dialog) = state.recording_dialog.take() {
            dialog.hide();
        }
        if let Some(recorder) = &state.recording {
            recorder.stop();
        }
        if let Some(cancel) = state.cancel.take() {
            let _ = cancel.send(true);
        }
        state.responses = None;
        stop_player(&mut state);
        self.drop_indicator.set_visible(false);
        self.picture.set_paintable(gtk::gdk::Paintable::NONE);
    }

    fn connect_camera(&self) {
        let source = self.source.borrow();
        let address = source.address.clone();
        let username = source.username.clone();
        let password = source.password.clone();
        let selected = source.selected_profile.clone();
        drop(source);
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let (response_tx, response_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("onvif-camera".into())
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
                    let profile = selected
                        .as_ref()
                        .and_then(|token| {
                            camera
                                .profiles
                                .iter()
                                .find(|profile| &profile.token == token)
                        })
                        .or_else(|| {
                            camera.profiles.iter().find(|profile| {
                                profile.codec.is_some()
                                    || profile.width.is_some()
                                    || profile.height.is_some()
                            })
                        })
                        .or_else(|| camera.profiles.first());
                    let Some(profile) = profile else {
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
                        Ok(uri) => Response::Stream {
                            uri,
                            profile: profile.name.clone(),
                        },
                        Err(error) => Response::Failed(error.to_string()),
                    };
                    let _ = response_tx.send(response);
                });
            });
        if worker.is_err() {
            self.status
                .set_text("Could not start the camera connection worker.");
            return;
        }
        let mut state = self.state.borrow_mut();
        state.cancel = Some(cancel_tx);
        state.responses = Some(response_rx);
        self.status.set_text("Connecting to camera…");
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
        let player = gst::ElementFactory::make("playbin")
            .property("video-sink", &sink)
            .property("uri", uri.as_str())
            .property("volume", self.volume.value())
            .property("mute", self.mute.is_active())
            .build()
            .map_err(|_| "The GStreamer playback plugin is not installed.")?;
        player.connect("source-setup", false, move |values| {
            if let Ok(source) = values[1].get::<gst::Element>() {
                if source
                    .factory()
                    .is_some_and(|factory| factory.name() == "rtspsrc")
                {
                    source.set_property("latency", RTSP_LATENCY_MS);
                    source.set_property("drop-on-latency", true);
                    source.connect("new-manager", false, |values| {
                        if let Ok(manager) = values[1].get::<gst::Element>() {
                            manager.connect("new-jitterbuffer", false, |values| {
                                if let Ok(buffer) = values[1].get::<gst::Element>() {
                                    buffer.set_property("post-drop-messages", true);
                                }
                                None
                            });
                        }
                        None
                    });
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
        Ok(())
    }

    fn poll(self: &Rc<Self>) -> glib::ControlFlow {
        if self.state.borrow().closed {
            return glib::ControlFlow::Break;
        }
        let response = {
            let state = self.state.borrow();
            state.responses.as_ref().map(Receiver::try_recv)
        };
        match response {
            Some(Ok(response)) => {
                self.state.borrow_mut().responses = None;
                match response {
                    Response::Stream { uri, profile } => {
                        let mut state = self.state.borrow_mut();
                        state.stream_uri = Some(uri.clone());
                        drop(state);
                        self.update_record_button();
                        self.title
                            .set_subtitle(&subtitle(&self.source.borrow().model, &profile));
                        match self.start_player(&uri) {
                            Ok(()) => {
                                self.status.set_visible(false);
                                self.update_snapshot_button();
                            }
                            Err(message) => self.status.set_text(message),
                        }
                    }
                    Response::Failed(message) => self.status.set_text(&message),
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.state.borrow_mut().responses = None;
                self.status
                    .set_text("Camera connection ended unexpectedly.");
            }
            _ => {}
        }
        let snapshot = {
            let state = self.state.borrow();
            state.snapshot_response.as_ref().map(Receiver::try_recv)
        };
        match snapshot {
            Some(Ok(result)) => {
                let mut state = self.state.borrow_mut();
                state.snapshot_response = None;
                drop(state);
                match result {
                    Ok(snapshot) => self.save_snapshot(snapshot),
                    Err(error) => {
                        self.update_snapshot_button();
                        self.snapshot_error("Could not capture snapshot", error);
                    }
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                let mut state = self.state.borrow_mut();
                state.snapshot_response = None;
                drop(state);
                self.update_snapshot_button();
                self.snapshot_error(
                    "Could not capture snapshot",
                    "The snapshot worker ended unexpectedly.",
                );
            }
            _ => {}
        }
        let recording_event = {
            let state = self.state.borrow();
            state
                .recording
                .as_ref()
                .map(|recording| recording.try_event())
        };
        match recording_event {
            Some(Ok(recording::Event::Started)) => {
                self.state.borrow_mut().recording_starting = false;
                self.update_record_button();
            }
            Some(Ok(recording::Event::Finished)) => {
                let mut state = self.state.borrow_mut();
                state.recording = None;
                state.recording_starting = false;
                state.recording_stopping = false;
                drop(state);
                self.update_record_button();
            }
            Some(Ok(recording::Event::Failed(error))) => {
                let mut state = self.state.borrow_mut();
                state.recording = None;
                state.recording_starting = false;
                state.recording_stopping = false;
                drop(state);
                self.update_record_button();
                self.recording_error(&error);
            }
            Some(Err(TryRecvError::Disconnected)) => {
                let mut state = self.state.borrow_mut();
                state.recording = None;
                state.recording_starting = false;
                state.recording_stopping = false;
                drop(state);
                self.update_record_button();
                self.recording_error("The recording worker ended unexpectedly.");
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        let mut failed = false;
        let mut dropped = 0_u64;
        if let Some(bus) = self
            .state
            .borrow()
            .player
            .as_ref()
            .and_then(|player| player.bus())
        {
            while let Some(message) = bus.pop() {
                match message.view() {
                    gst::MessageView::Error(_) | gst::MessageView::Eos(_) => {
                        failed = true;
                        break;
                    }
                    gst::MessageView::Element(element) => {
                        if let Some(details) =
                            element.structure().filter(|s| s.name() == "drop-msg")
                        {
                            dropped += u64::from(details.get::<u32>("num-too-late").unwrap_or(0));
                            dropped +=
                                u64::from(details.get::<u32>("num-drop-on-latency").unwrap_or(0));
                        }
                    }
                    _ => {}
                }
            }
        }
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        if state
            .last_drop
            .is_some_and(|last| now.duration_since(last) >= DROP_INDICATOR_DURATION)
        {
            state.last_drop = None;
            state.dropped_packets = 0;
        }
        if dropped > 0 {
            state.last_drop = Some(now);
            state.dropped_packets = state.dropped_packets.saturating_add(dropped);
            self.drop_indicator
                .set_label(&format!("Packets dropped: {}", state.dropped_packets));
        }
        self.drop_indicator
            .set_visible(state.last_drop.is_some() && !failed);
        drop(state);
        if failed {
            self.cancel_snapshot();
            stop_player(&mut self.state.borrow_mut());
            self.picture.set_paintable(gtk::gdk::Paintable::NONE);
            self.status.set_text(
                "The stream ended or could not be played. Check the camera and GStreamer plugins.",
            );
            self.status.set_visible(true);
        }
        glib::ControlFlow::Continue
    }
}

fn subtitle(model: &str, profile: &str) -> String {
    [model, profile]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" • ")
}
// Fit the initial video area within a sensible desktop-sized bounding box.
// GTK includes the header bar in the window's default height.
fn initial_video_size(width: Option<u32>, height: Option<u32>) -> (i32, i32) {
    const MAX_WIDTH: u64 = 900;
    const MAX_HEIGHT: u64 = 560;
    let (Some(width), Some(height)) = (width, height) else {
        return (896, 504);
    };
    if width == 0 || height == 0 {
        return (896, 504);
    }
    let (width, height) = (u64::from(width), u64::from(height));
    let (mut a, mut b) = (width, height);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    let (ratio_width, ratio_height) = (width / a, height / a);
    let scale = (MAX_WIDTH / ratio_width).min(MAX_HEIGHT / ratio_height);
    if scale > 0 {
        return ((ratio_width * scale) as i32, (ratio_height * scale) as i32);
    }
    if width * MAX_HEIGHT >= height * MAX_WIDTH {
        (
            MAX_WIDTH as i32,
            ((height * MAX_WIDTH + width / 2) / width).max(1) as i32,
        )
    } else {
        (
            ((width * MAX_HEIGHT + height / 2) / height).max(1) as i32,
            MAX_HEIGHT as i32,
        )
    }
}

pub fn open(
    parent: &adw::ApplicationWindow,
    source: Source,
    on_select: impl Fn(String) + 'static,
) -> adw::Window {
    let profile = source
        .selected_profile
        .as_ref()
        .and_then(|token| {
            source
                .profiles
                .iter()
                .find(|profile| &profile.token == token)
        })
        .or_else(|| {
            source.profiles.iter().find(|profile| {
                profile.codec.is_some() || profile.width.is_some() || profile.height.is_some()
            })
        })
        .or_else(|| source.profiles.first());
    let (video_width, video_height) = initial_video_size(
        profile.and_then(|profile| profile.width),
        profile.and_then(|profile| profile.height),
    );
    let title = adw::WindowTitle::new(
        &source.name,
        &subtitle(&source.model, profile.map_or("", |profile| &profile.name)),
    );
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    let drop_indicator = gtk::Label::new(None);
    drop_indicator.set_visible(false);
    drop_indicator.set_tooltip_text(Some(
        "RTP packets reported dropped by the jitter buffer; this does not measure camera delay.",
    ));
    header.pack_end(&drop_indicator);
    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    picture.set_keep_aspect_ratio(true);
    picture.set_vexpand(true);
    picture.set_hexpand(true);
    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_halign(gtk::Align::Center);
    status.set_valign(gtk::Align::Center);
    status.add_css_class("dim-label");
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&status);
    let layout = adw::ToolbarView::new();
    layout.add_top_bar(&header);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    toolbar.add_css_class("grid-controls");
    toolbar.set_halign(gtk::Align::Fill);
    toolbar.set_valign(gtk::Align::End);
    toolbar.set_margin_bottom(8);
    toolbar.set_margin_start(8);
    toolbar.set_margin_end(8);
    let mute = gtk::ToggleButton::new();
    mute.set_icon_name("audio-volume-high-symbolic");
    mute.set_tooltip_text(Some("Mute"));
    mute.add_css_class("flat");
    mute.add_css_class("circular");
    toolbar.append(&mute);
    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    volume.set_value(1.0);
    volume.set_draw_value(false);
    volume.set_size_request(70, -1);
    volume.set_hexpand(true);
    volume.set_tooltip_text(Some("Volume"));
    toolbar.append(&volume);
    let choices = gtk::Box::new(gtk::Orientation::Vertical, 3);
    choices.set_margin_top(6);
    choices.set_margin_bottom(6);
    choices.set_margin_start(6);
    choices.set_margin_end(6);
    let profile_popover = gtk::Popover::new();
    profile_popover.set_child(Some(&choices));
    let profiles = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .popover(&profile_popover)
        .tooltip_text("Select Profile")
        .build();
    profiles.add_css_class("flat");
    profiles.add_css_class("circular");
    toolbar.append(&profiles);
    let snapshot_button = gtk::Button::from_icon_name("camera-photo-symbolic");
    snapshot_button.set_tooltip_text(Some("Take snapshot"));
    snapshot_button.add_css_class("flat");
    snapshot_button.add_css_class("circular");
    snapshot_button.set_sensitive(false);
    toolbar.append(&snapshot_button);
    let record_button = gtk::Button::from_icon_name("media-record-symbolic");
    record_button.set_tooltip_text(Some("Record clip"));
    record_button.add_css_class("flat");
    record_button.add_css_class("circular");
    record_button.set_sensitive(false);
    toolbar.append(&record_button);
    overlay.add_overlay(&toolbar);
    layout.set_content(Some(&overlay));
    let window = adw::Window::builder()
        .transient_for(parent)
        .destroy_with_parent(true)
        .modal(false)
        .title(&source.name)
        .default_width(video_width)
        .default_height(video_height)
        .content(&layout)
        .build();
    let (_, header_height, _, _) = header.measure(gtk::Orientation::Vertical, video_width);
    window.set_default_size(video_width, video_height + header_height);
    let token = source
        .selected_profile
        .as_ref()
        .filter(|token| {
            source
                .profiles
                .iter()
                .any(|profile| &profile.token == *token)
        })
        .cloned()
        .or_else(|| Source::default_profile(&source.profiles));
    let window_weak = window.downgrade();
    let ptz = PtzControl::new(source.clone(), token, move |error| {
        if let Some(window) = window_weak.upgrade() {
            let dialog = adw::MessageDialog::new(
                Some(&window),
                Some("Could not control camera"),
                Some(&error),
            );
            dialog.add_response("close", "Close");
            dialog.present();
        }
    });
    toolbar.insert_child_after(&ptz.button(), Some(&volume));
    let ui = Rc::new(Ui {
        source: RefCell::new(source),
        title,
        status,
        drop_indicator,
        picture,
        window: window.downgrade(),
        snapshot_button: snapshot_button.clone(),
        record_button: record_button.clone(),
        ptz,
        mute: mute.clone(),
        volume: volume.clone(),
        choices,
        profile_popover,
        on_select: Rc::new(on_select),
        state: RefCell::new(State::default()),
    });
    ui.update_profiles();
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&ui);
    keys.connect_key_pressed(move |_, key, _, _| {
        if weak
            .upgrade()
            .is_some_and(|ui| ui.ptz.handle_key(key, true))
        {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    let weak = Rc::downgrade(&ui);
    keys.connect_key_released(move |_, key, _, _| {
        if let Some(ui) = weak.upgrade() {
            ui.ptz.handle_key(key, false);
        }
    });
    window.add_controller(keys);
    let weak = Rc::downgrade(&ui);
    window.connect_is_active_notify(move |window| {
        if !window.is_active()
            && let Some(ui) = weak.upgrade()
        {
            ui.ptz.deactivate();
        }
    });
    let weak = Rc::downgrade(&ui);
    mute.connect_toggled(move |button| {
        let muted = button.is_active();
        let label = if muted { "Unmute" } else { "Mute" };
        button.set_icon_name(if muted {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
        button.set_tooltip_text(Some(label));
        if let Some(ui) = weak.upgrade()
            && let Some(player) = &ui.state.borrow().player
        {
            player.set_property("mute", muted);
        }
    });
    let weak = Rc::downgrade(&ui);
    volume.connect_value_changed(move |volume| {
        if let Some(ui) = weak.upgrade()
            && let Some(player) = &ui.state.borrow().player
        {
            player.set_property("volume", volume.value());
        }
    });
    let weak = Rc::downgrade(&ui);
    snapshot_button.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.capture();
        }
    });
    let weak = Rc::downgrade(&ui);
    record_button.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.choose_recording_destination();
        }
    });
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        windows.retain(|(window, ui)| window.upgrade().is_some() && ui.upgrade().is_some());
        windows.push((window.downgrade(), Rc::downgrade(&ui)));
    });
    let weak = Rc::downgrade(&ui);
    window.connect_close_request(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.close();
        }
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&ui);
    window.connect_hide(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.close();
        }
    });
    ui.connect_camera();
    glib::timeout_add_local(Duration::from_millis(100), move || ui.poll());
    window.present();
    window
}

pub fn change_profile(window: &adw::Window, source: Source) {
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        windows.retain(|(window, ui)| window.upgrade().is_some() && ui.upgrade().is_some());
        if let Some(ui) = windows.iter().find_map(|(candidate, ui)| {
            (candidate.upgrade().as_ref() == Some(window))
                .then(|| ui.upgrade())
                .flatten()
        }) {
            ui.ptz.deactivate();
            ui.cancel_snapshot();
            {
                let mut state = ui.state.borrow_mut();
                if state.closed {
                    return;
                }
                if let Some(cancel) = state.cancel.take() {
                    let _ = cancel.send(true);
                }
                state.responses = None;
                state.stream_uri = None;
                stop_player(&mut state);
            }
            ui.drop_indicator.set_visible(false);
            ui.picture.set_paintable(gtk::gdk::Paintable::NONE);
            let profile = source
                .selected_profile
                .as_ref()
                .and_then(|token| {
                    source
                        .profiles
                        .iter()
                        .find(|profile| &profile.token == token)
                })
                .or_else(|| {
                    source.profiles.iter().find(|profile| {
                        profile.codec.is_some()
                            || profile.width.is_some()
                            || profile.height.is_some()
                    })
                })
                .or_else(|| source.profiles.first());
            ui.title.set_title(&source.name);
            ui.title.set_subtitle(&subtitle(
                &source.model,
                profile.map_or("", |profile| &profile.name),
            ));
            *ui.source.borrow_mut() = source;
            let source = ui.source.borrow().clone();
            let token = source
                .selected_profile
                .as_ref()
                .filter(|token| {
                    source
                        .profiles
                        .iter()
                        .any(|profile| &profile.token == *token)
                })
                .cloned()
                .or_else(|| Source::default_profile(&source.profiles));
            ui.ptz.configure(source, token);
            ui.update_profiles();
            ui.status.set_visible(true);
            ui.connect_camera();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{capture_frame, initial_video_size, suggested_filename};
    use gst::prelude::*;
    use gstreamer as gst;

    #[test]
    fn captures_decoded_frame_as_jpeg() {
        gst::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.avi");
        let generator = gst::parse::launch(
            "videotestsrc num-buffers=10 ! video/x-raw,width=64,height=48,framerate=10/1 ! jpegenc ! avimux ! filesink name=output",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        generator
            .by_name("output")
            .unwrap()
            .set_property("location", path.to_str().unwrap());
        generator.set_state(gst::State::Playing).unwrap();
        let message = generator
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("video generation timed out");
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "{message:?}"
        );
        generator.set_state(gst::State::Null).unwrap();

        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        let player = gst::ElementFactory::make("playbin")
            .property("uri", url::Url::from_file_path(path).unwrap().as_str())
            .property("video-sink", sink)
            .build()
            .unwrap();
        assert!(capture_frame(&player).is_err());
        player.set_state(gst::State::Paused).unwrap();
        let (result, current, _) = player.state(gst::ClockTime::from_seconds(10));
        result.unwrap();
        assert_eq!(current, gst::State::Paused);
        let jpeg = capture_frame(&player).unwrap();
        assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]));
        assert!(jpeg.ends_with(&[0xff, 0xd9]));
        player.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn capture_filenames_include_safe_camera_name_and_timestamp() {
        assert_eq!(
            suggested_filename(
                "Front / Door",
                "recording",
                "mkv",
                "2026-11-21_13-05-09.123"
            ),
            "Front---Door-recording-2026-11-21_13-05-09.123.mkv"
        );
        assert_eq!(
            suggested_filename("", "snapshot", "jpg", "2026-11-21_13-05-09.123"),
            "camera-snapshot-2026-11-21_13-05-09.123.jpg"
        );
    }

    #[test]
    fn initial_video_size_fits_portrait_and_landscape_profiles() {
        assert_eq!(initial_video_size(Some(2304), Some(1296)), (896, 504));
        assert_eq!(initial_video_size(Some(720), Some(1280)), (315, 560));
        assert_eq!(initial_video_size(Some(1000), Some(1000)), (560, 560));
        assert_eq!(initial_video_size(Some(u32::MAX), Some(1)), (900, 1));
        assert_eq!(initial_video_size(Some(0), Some(720)), (896, 504));
        assert_eq!(initial_video_size(None, None), (896, 504));
    }
}
