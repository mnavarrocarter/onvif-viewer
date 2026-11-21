use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use adw::prelude::*;
use adw::{glib, gtk};
use tokio::sync::watch;

use crate::storage::Source;

const PTZ_RENEW_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct PtzTarget {
    source: Source,
    token: String,
    generation: u64,
}

#[derive(Clone, Default)]
struct PtzRequest {
    target: Option<PtzTarget>,
    direction: (f32, f32),
    shutdown: bool,
}

enum PtzEvent {
    Supported(u64, bool),
    Error(u64, String),
}

// One worker serializes movement and stop requests across profile changes.
async fn run_ptz(mut requests: watch::Receiver<PtzRequest>, events: mpsc::Sender<PtzEvent>) {
    let mut current: Option<(onvif::Camera, String, u64)> = None;
    let mut moving = false;
    let mut failed_generation = None;
    loop {
        let request = requests.borrow_and_update().clone();
        let generation = request.target.as_ref().map(|target| target.generation);
        if current.as_ref().map(|(_, _, generation)| *generation) != generation || request.shutdown
        {
            if let Some((camera, token, old_generation)) = current.take()
                && moving
            {
                if let Err(error) = camera.ptz_stop(&token).await {
                    let _ = events.send(PtzEvent::Error(old_generation, error.to_string()));
                }
                moving = false;
            }
            if request.shutdown {
                break;
            }
        }
        if current.is_none()
            && failed_generation != generation
            && let Some(target) = &request.target
        {
            let result = async {
                let camera = onvif::connect(
                    &target.source.address,
                    &target.source.username,
                    &target.source.password,
                )
                .await?;
                let supported = camera.ptz_supported(&target.token).await?;
                Ok::<_, onvif::Error>((camera, supported))
            }
            .await;
            match result {
                Ok((camera, supported)) => {
                    let _ = events.send(PtzEvent::Supported(target.generation, supported));
                    if supported {
                        current = Some((camera, target.token.clone(), target.generation));
                    } else {
                        failed_generation = generation;
                        let _ = events.send(PtzEvent::Error(target.generation, error.to_string()));
                    }
                }
                continue;
            }
        }
        if let Some((camera, token, generation)) = &current {
            let result = if request.direction != (0.0, 0.0) {
                moving = true;
                camera
                    .ptz_move(token, request.direction.0, request.direction.1)
                    .await
            } else if moving {
                moving = false;
                camera.ptz_stop(token).await
            } else {
                if requests.changed().await.is_err() {
                    break;
                }
                continue;
            };
            if let Err(error) = result {
                // A failed move may still have reached the camera.
                if moving {
                    if let Err(stop_error) = camera.ptz_stop(token).await {
                        let _ = events.send(PtzEvent::Error(*generation, stop_error.to_string()));
                    }
                    moving = false;
                }
                failed_generation = Some(*generation);
                let _ = events.send(PtzEvent::Error(*generation, error.to_string()));
                current = None;
                continue;
            }
        }
        if moving {
            tokio::select! {
                _ = tokio::time::sleep(PTZ_RENEW_INTERVAL) => {},
                changed = requests.changed() => if changed.is_err() { break; },
            }
        } else if requests.changed().await.is_err() {
            break;
        }
    }
}

pub(crate) struct PtzControl {
    button: gtk::ToggleButton,
    request: Option<watch::Sender<PtzRequest>>,
    events: RefCell<Option<Receiver<PtzEvent>>>,
    generation: Cell<u64>,
    keys: Cell<[bool; 4]>,
    stopped: Cell<bool>,
    on_error: Box<dyn Fn(String)>,
}

impl PtzControl {
    pub(crate) fn new(
        source: Source,
        token: Option<String>,
        on_error: impl Fn(String) + 'static,
    ) -> Rc<Self> {
        let button = gtk::ToggleButton::new();
        button.add_css_class("flat");
        button.add_css_class("circular");
        button.set_sensitive(false);
        let icon = gtk::DrawingArea::new();
        icon.set_content_width(16);
        icon.set_content_height(16);
        let button_weak = button.downgrade();
        icon.set_draw_func(move |_, cr, width, height| {
            if button_weak
                .upgrade()
                .is_some_and(|button| button.is_active())
            {
                cr.set_source_rgb(0.2, 0.9, 0.35);
            } else {
                cr.set_source_rgb(1.0, 1.0, 1.0);
            }
            cr.set_line_width(1.7);
            cr.set_line_cap(gtk::cairo::LineCap::Round);
            cr.set_line_join(gtk::cairo::LineJoin::Round);
            let x = f64::from(width) / 2.0;
            let y = f64::from(height) / 2.0;
            cr.move_to(x - 6.0, y);
            cr.line_to(x + 6.0, y);
            cr.move_to(x, y - 6.0);
            cr.line_to(x, y + 6.0);
            for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
                let (tip_x, tip_y) = (x + dx * 6.0, y + dy * 6.0);
                cr.move_to(tip_x - dx * 2.5 - dy * 2.5, tip_y - dy * 2.5 + dx * 2.5);
                cr.line_to(tip_x, tip_y);
                cr.line_to(tip_x - dx * 2.5 + dy * 2.5, tip_y - dy * 2.5 - dx * 2.5);
            }
            let _ = cr.stroke();
        });
        button.set_child(Some(&icon));
        let (request_tx, request_rx) = watch::channel(PtzRequest::default());
        let (event_tx, event_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("camera-ptz".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(run_ptz(request_rx, event_tx)),
                    Err(error) => {
                        let _ = event_tx.send(PtzEvent::Error(0, error.to_string()));
                    }
                }
            });
        let started = worker.is_ok();
        let control = Rc::new(Self {
            button: button.clone(),
            request: started.then_some(request_tx),
            events: RefCell::new(started.then_some(event_rx)),
            generation: Cell::new(0),
            keys: Cell::new([false; 4]),
            stopped: Cell::new(false),
            on_error: Box::new(on_error),
        });
        let weak = Rc::downgrade(&control);
        button.connect_toggled(move |button| {
            icon.queue_draw();
            if !button.is_active()
                && let Some(control) = weak.upgrade()
            {
                control.clear_keys();
            }
        });
        control.configure(source, token);
        if !started {
            (control.on_error)("Could not start the pan/tilt worker.".into());
        }
        let weak = Rc::downgrade(&control);
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let Some(control) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if control.stopped.get() {
                return glib::ControlFlow::Break;
            }
            control.poll();
            glib::ControlFlow::Continue
        });
        control
    }

    pub(crate) fn button(&self) -> gtk::ToggleButton {
        self.button.clone()
    }

    pub(crate) fn configure(&self, source: Source, token: Option<String>) {
        self.deactivate();
        self.button.set_sensitive(false);
        self.button
            .set_tooltip_text(Some("Checking pan/tilt support…"));
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        if !self.stopped.get()
            && let Some(sender) = &self.request
        {
                sender.send_modify(|request| {
                    request.direction = (0.0, 0.0);
                    request.target = token.map(|token| PtzTarget {
                        source,
                        token,
                        generation,
                    });
                });
        }
    }

    fn clear_keys(&self) {
        self.keys.set([false; 4]);
        if let Some(sender) = &self.request {
            sender.send_modify(|request| request.direction = (0.0, 0.0));
        }
    }

    pub(crate) fn deactivate(&self) {
        self.button.set_active(false);
        self.clear_keys();
    }

    pub(crate) fn shutdown(&self) {
        if !self.stopped.replace(true) {
            self.deactivate();
            self.button.set_sensitive(false);
            if let Some(sender) = &self.request {
                sender.send_modify(|request| request.shutdown = true);
            }
        }
    }

    pub(crate) fn handle_key(&self, key: gtk::gdk::Key, pressed: bool) -> bool {
        let index = match key {
            gtk::gdk::Key::Left => 0,
            gtk::gdk::Key::Right => 1,
            gtk::gdk::Key::Up => 2,
            gtk::gdk::Key::Down => 3,
            _ => return false,
        };
        if !self.button.is_active() {
            return false;
        }
        let mut keys = self.keys.get();
        if keys[index] != pressed {
            keys[index] = pressed;
            self.keys.set(keys);
            let direction = (
                (i32::from(keys[1]) - i32::from(keys[0])) as f32 * 0.5,
                (i32::from(keys[2]) - i32::from(keys[3])) as f32 * 0.5,
            );
            if let Some(sender) = &self.request {
                sender.send_modify(|request| request.direction = direction);
            }
        }
        true
    }

    fn poll(&self) {
        loop {
            let event = self.events.borrow().as_ref().map(Receiver::try_recv);
            match event {
                Some(Ok(PtzEvent::Supported(generation, supported))) => {
                    if generation == self.generation.get() {
                        self.button.set_sensitive(supported);
                        self.button.set_tooltip_text(Some(if supported {
                            "Enable arrow-key pan and tilt"
                        } else {
                            "Pan and tilt are not supported for this profile"
                        }));
                    }
                }
                Some(Ok(PtzEvent::Error(generation, error))) => {
                    if generation == self.generation.get() || generation == 0 {
                        self.deactivate();
                        self.button.set_sensitive(false);
                    }
                    (self.on_error)(error);
                }
                Some(Err(TryRecvError::Disconnected)) => {
                    *self.events.borrow_mut() = None;
                    self.deactivate();
                    self.button.set_sensitive(false);
                    (self.on_error)("The pan/tilt worker ended unexpectedly.".into());
                    break;
                }
                _ => break,
            }
        }
    }
}

impl Drop for PtzControl {
    fn drop(&mut self) {
        self.shutdown();
    }
}
