use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use adw::prelude::*;
use adw::{glib, gtk};
use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::watch;

use crate::storage::Source;

enum Response {
    Stream { uri: String, profile: String },
    Failed(String),
}

#[derive(Default)]
struct State {
    cancel: Option<watch::Sender<bool>>,
    responses: Option<Receiver<Response>>,
    closed: bool,
    player: Option<gst::Element>,
}

struct Ui {
    source: RefCell<Source>,
    title: adw::WindowTitle,
    status: gtk::Label,
    picture: gtk::Picture,
    state: RefCell<State>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<(glib::WeakRef<adw::Window>, std::rc::Weak<Ui>)>> =
        RefCell::new(Vec::new());
}

fn stop_player(state: &mut State) {
    if let Some(player) = state.player.take() {
        std::thread::spawn(move || {
            let _ = player.set_state(gst::State::Null);
        });
    }
}

impl Ui {
    fn close(&self) {
        let mut state = self.state.borrow_mut();
        state.closed = true;
        if let Some(cancel) = state.cancel.take() {
            let _ = cancel.send(true);
        }
        state.responses = None;
        stop_player(&mut state);
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
            .property("volume", 1.0_f64)
            .property("mute", false)
            .build()
            .map_err(|_| "The GStreamer playback plugin is not installed.")?;
        player.connect("source-setup", false, move |values| {
            if let Ok(source) = values[1].get::<gst::Element>() {
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

    fn poll(&self) -> glib::ControlFlow {
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
                        self.title
                            .set_subtitle(&subtitle(&self.source.borrow().model, &profile));
                        match self.start_player(&uri) {
                            Ok(()) => self.status.set_visible(false),
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
        let failed = {
            let state = self.state.borrow();
            state
                .player
                .as_ref()
                .and_then(|player| player.bus())
                .is_some_and(|bus| {
                    while let Some(message) = bus.pop() {
                        if matches!(
                            message.view(),
                            gst::MessageView::Error(_) | gst::MessageView::Eos(_)
                        ) {
                            return true;
                        }
                    }
                    false
                })
        };
        if failed {
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

#[cfg(test)]
mod tests {
    use super::initial_video_size;

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

pub fn open(parent: &adw::ApplicationWindow, source: Source) -> adw::Window {
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
    let ui = Rc::new(Ui {
        source: RefCell::new(source),
        title,
        status,
        picture,
        state: RefCell::new(State::default()),
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
            {
                let mut state = ui.state.borrow_mut();
                if state.closed {
                    return;
                }
                if let Some(cancel) = state.cancel.take() {
                    let _ = cancel.send(true);
                }
                state.responses = None;
                stop_player(&mut state);
            }
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
            ui.status.set_visible(true);
            ui.connect_camera();
        }
    });
}
