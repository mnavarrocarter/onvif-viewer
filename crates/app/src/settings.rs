use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use adw::{glib, gtk};
use tokio::sync::watch;

use crate::storage::Source;

struct Settings {
    window: adw::Window,
    name: adw::EntryRow,
    address: adw::EntryRow,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    save: gtk::Button,
    remove: gtk::Button,
    status: gtk::Label,
    spinner: gtk::Spinner,
    cancel: RefCell<Option<watch::Sender<bool>>>,
    closed: Cell<bool>,
}

impl Settings {
    fn set_busy(&self, busy: bool) {
        self.name.set_sensitive(!busy);
        self.address.set_sensitive(!busy);
        self.username.set_sensitive(!busy);
        self.password.set_sensitive(!busy);
        self.save.set_sensitive(!busy);
        self.remove.set_sensitive(!busy);
        if busy {
            self.status.remove_css_class("error");
            self.status.set_text("Connecting to camera…");
            self.spinner.start();
        } else {
            self.spinner.stop();
        }
    }

    fn show_error(&self, message: &str) {
        self.cancel.borrow_mut().take();
        self.set_busy(false);
        self.status.add_css_class("error");
        self.status.set_text(message);
    }

    fn finish_operation(self: &Rc<Self>, rx: mpsc::Receiver<Result<(), String>>) {
        self.cancel.borrow_mut().take();
        self.set_busy(true);
        self.status.remove_css_class("error");
        self.status.set_text("Saving camera library…");
        let weak = Rc::downgrade(self);
        crate::await_store(rx, move |result| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.closed.get() {
                return;
            }
            match result {
                Ok(()) => ui.window.close(),
                Err(error) => ui.show_error(&error),
            }
        });
    }
}

// Only display messages we control, never potentially sensitive device responses.
pub(crate) fn connection_error(error: &onvif::Error) -> &'static str {
    match error.to_string().as_str() {
        "Invalid camera address; enter a host or HTTP(S) URL" => {
            "Invalid camera address; enter a host or HTTP(S) URL."
        }
        "Camera service address must be an HTTP(S) URL" => {
            "The camera service address must be an HTTP(S) URL."
        }
        "Do not include credentials in the camera service address" => {
            "Remove credentials from the address and enter them in the username and password fields."
        }
        "Camera request timed out; check its address and network connection" => {
            "Camera request timed out. Check its address and network connection."
        }
        "Could not reach the camera; check its address and network connection" => {
            "Could not reach the camera. Check its address and network connection."
        }
        "Camera rejected authentication; check the username, password, and camera clock" => {
            "Camera rejected authentication. Check the username, password, and camera clock."
        }
        "Camera returned an ONVIF fault; check credentials, camera clock, and service support" => {
            "Camera returned an ONVIF fault. Check the credentials, camera clock, and ONVIF support."
        }
        _ => {
            "Could not connect to the camera. Check the address, credentials, and ONVIF settings, then try again."
        }
    }
}

pub fn open(
    parent: &adw::ApplicationWindow,
    source: Source,
    on_save: impl Fn(Source) -> mpsc::Receiver<Result<(), String>> + 'static,
    on_remove: impl Fn(u64) -> mpsc::Receiver<Result<(), String>> + 'static,
) {
    let name = adw::EntryRow::builder().title("Name").build();
    name.set_text(&source.name);
    let address = adw::EntryRow::builder()
        .title("Device Service Address")
        .build();
    address.set_text(&source.address);
    let username = adw::EntryRow::builder().title("Username").build();
    username.set_text(&source.username);
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    password.set_text(&source.password);

    let group = adw::PreferencesGroup::new();
    group.set_description(Some(
        "Enter the camera’s ONVIF device service address, including its port.",
    ));
    group.add(&name);
    group.add(&address);
    group.add(&username);
    group.add(&password);
    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(0.0);
    let spinner = gtk::Spinner::new();
    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_row.append(&spinner);
    status_row.append(&status);
    let remove = gtk::Button::with_label("Remove Camera");
    remove.add_css_class("destructive-action");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    body.set_margin_top(18);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.append(&group);
    body.append(&status_row);
    body.append(&remove);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&body)
        .build();
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    let header = adw::HeaderBar::new();
    header.pack_end(&save);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));
    let window = adw::Window::builder()
        .title("Camera Settings")
        .transient_for(parent)
        .destroy_with_parent(true)
        .modal(true)
        .default_width(440)
        .default_height(420)
        .content(&toolbar)
        .build();
    let ui = Rc::new(Settings {
        window,
        name,
        address,
        username,
        password,
        save,
        remove,
        status,
        spinner,
        cancel: RefCell::new(None),
        closed: Cell::new(false),
    });

    let weak = Rc::downgrade(&ui);
    ui.window.connect_destroy(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.closed.set(true);
            if let Some(cancel) = ui.cancel.borrow_mut().take() {
                let _ = cancel.send(true);
            }
        }
    });
    let weak = Rc::downgrade(&ui);
    let id = source.id;
    ui.remove.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.finish_operation(on_remove(id));
        }
    });
    let weak = Rc::downgrade(&ui);
    let on_save = Rc::new(on_save);
    ui.save.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        let name = ui.name.text().trim().to_owned();
        if name.is_empty() {
            ui.show_error("Enter a name for the camera.");
            return;
        }
        let address = ui.address.text().trim().to_owned();
        if address.is_empty() {
            ui.show_error("Enter the camera’s device service address.");
            return;
        }
        let mut updated = source.clone();
        updated.name = name;
        updated.address = address;
        updated.username = ui.username.text().to_string();
        updated.password = ui.password.text().to_string();
        if updated.address == source.address
            && updated.username == source.username
            && updated.password == source.password
        {
            ui.finish_operation(on_save(updated));
            return;
        }
        let worker_address = updated.address.clone();
        let worker_username = updated.username.clone();
        let worker_password = updated.password.clone();
        let (cancel, mut cancel_rx) = watch::channel(false);
        let (tx, rx) = mpsc::channel();
        if std::thread::Builder::new().name("onvif-settings".into()).spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => {
                    let Some(result) = runtime.block_on(async {
                        tokio::select! {
                            biased;
                            _ = cancel_rx.changed() => None,
                            result = onvif::connect(&worker_address, &worker_username, &worker_password) => {
                                Some(result.map(|camera| (camera.information.model, camera.information.serial_number, camera.profiles))
                                    .map_err(|error| connection_error(&error)))
                            }
                        }
                    }) else { return };
                    result
                }
                Err(_) => Err("Could not initialize the camera connection. Try again."),
            };
            let _ = tx.send(result);
        }).is_err() {
            ui.show_error("Could not start the camera connection. Try again.");
            return;
        }
        *ui.cancel.borrow_mut() = Some(cancel);
        ui.set_busy(true);
        let weak = Rc::downgrade(&ui);
        let on_save = Rc::clone(&on_save);
        let mut updated = Some(updated);
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let Some(ui) = weak.upgrade() else { return glib::ControlFlow::Break };
            if ui.closed.get() { return glib::ControlFlow::Break; }
            match rx.try_recv() {
                Ok(Ok((model, serial, profiles))) => {
                    let Some(mut updated) = updated.take() else { return glib::ControlFlow::Break };
                    updated.model = model;
                    updated.serial = serial;
                    updated.selected_profile = updated.selected_profile.as_ref()
                        .filter(|token| profiles.iter().any(|profile| &profile.token == *token))
                        .cloned()
                        .or_else(|| profiles.iter()
                            .find(|profile| profile.codec.is_some() || profile.width.is_some() || profile.height.is_some())
                            .or_else(|| profiles.first())
                            .map(|profile| profile.token.clone()));
                    updated.profiles = profiles;
                    ui.finish_operation(on_save(updated));
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    ui.show_error(error);
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    ui.show_error("Camera connection ended unexpectedly. Try again.");
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            }
        });
    });
    // Keep the state alive until the window is destroyed without creating a reference cycle.
    let keep_alive = Rc::clone(&ui);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if keep_alive.closed.get() {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
    ui.window.present();
}
