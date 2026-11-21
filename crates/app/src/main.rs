mod feed;
mod settings;
mod storage;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::glib;
use adw::gtk;
use adw::prelude::*;
use adw::{Application, ApplicationWindow};
use storage::{PersistentStore, Source, SourceStore};
use tokio::sync::watch;

const APP_ID: &str = "io.github.onvif_viewer.OnvifViewer";

type StoreResult<T> = Result<T, String>;

enum StoreRequest {
    Load(mpsc::Sender<StoreResult<Vec<Source>>>),
    Insert(Source, mpsc::Sender<StoreResult<()>>),
    Update(Source, mpsc::Sender<StoreResult<()>>),
    Remove(u64, mpsc::Sender<StoreResult<()>>),
}

fn start_store_worker() -> Option<mpsc::Sender<StoreRequest>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("camera-library".into())
        .spawn(move || {
            let mut store: Option<PersistentStore> = None;
            while let Ok(request) = rx.recv() {
                match request {
                    StoreRequest::Load(reply) => {
                        let result = (|| {
                            if store.is_none() {
                                store = Some(PersistentStore::open().map_err(|e| e.to_string())?);
                            }
                            store.as_ref().unwrap().list().map_err(|e| e.to_string())
                        })();
                        let _ = reply.send(result);
                    }
                    StoreRequest::Insert(source, reply) => {
                        let result = store.as_mut()
                            .ok_or_else(|| "Camera library is not available. Retry loading it.".to_owned())
                            .and_then(|store| store.insert(source).map_err(|e| e.to_string()));
                        let _ = reply.send(result);
                    }
                    StoreRequest::Update(source, reply) => {
                        let result = store.as_mut()
                            .ok_or_else(|| "Camera library is not available. Retry loading it.".to_owned())
                            .and_then(|store| store.update(source).map_err(|e| e.to_string()));
                        let _ = reply.send(result);
                    }
                    StoreRequest::Remove(id, reply) => {
                        let result = store.as_mut()
                            .ok_or_else(|| "Camera library is not available. Retry loading it.".to_owned())
                            .and_then(|store| store.remove(id).map_err(|e| e.to_string()));
                        let _ = reply.send(result);
                    }
                }
            }
        })
        .ok()?;
    Some(tx)
}

fn await_store<T: 'static>(rx: mpsc::Receiver<StoreResult<T>>, callback: impl FnOnce(StoreResult<T>) + 'static) {
    let mut callback = Some(callback);
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => Err("Camera library worker stopped. Restart the application and try again.".to_owned()),
        };
        callback.take().unwrap()(result);
        glib::ControlFlow::Break
    });
}

fn failed_operation(message: &str) -> mpsc::Receiver<StoreResult<()>> {
    let (tx, rx) = mpsc::channel();
    let _ = tx.send(Err(message.to_owned()));
    rx
}

fn same_source(a: &Source, b: &Source) -> bool {
    a.id == b.id
        && a.name == b.name
        && a.address == b.address
        && a.username == b.username
        && a.password == b.password
        && a.model == b.model
        && a.serial == b.serial
        && a.selected_profile == b.selected_profile
        && a.profiles.len() == b.profiles.len()
        && a.profiles.iter().zip(&b.profiles).all(|(a, b)| {
            a.token == b.token && a.name == b.name && a.width == b.width
                && a.height == b.height && a.codec == b.codec && a.audio_codec == b.audio_codec
        })
}

struct Library {
    window: glib::WeakRef<ApplicationWindow>,
    stack: gtk::Stack,
    grid: gtk::FlowBox,
    toasts: adw::ToastOverlay,
    header_add: glib::WeakRef<gtk::Button>,
    store: Option<mpsc::Sender<StoreRequest>>,
    sources: RefCell<Vec<Source>>,
    busy: Cell<bool>,
    ready: Cell<bool>,
    feeds: RefCell<HashMap<u64, Vec<glib::WeakRef<adw::Window>>>>,
    next_id: Cell<Option<u64>>,
    loading: Cell<bool>,
    load_status: gtk::Label,
    load_retry: gtk::Button,
    empty_add: gtk::Button,
}

impl Library {
    fn refresh(self: &Rc<Self>) {
        let sources = self.sources.borrow().clone();
        while let Some(child) = self.grid.first_child() {
            self.grid.remove(&child);
        }
        if let Some(button) = self.header_add.upgrade() {
            button.set_visible(!sources.is_empty());
        }
        self.stack.set_visible_child_name(if sources.is_empty() {
            "empty"
        } else {
            "sources"
        });
        for source in sources {
            let contents = gtk::Box::new(gtk::Orientation::Vertical, 6);
            contents.set_valign(gtk::Align::Center);
            contents.set_margin_top(22);
            contents.set_margin_bottom(54);
            contents.set_margin_start(18);
            contents.set_margin_end(18);
            let title = gtk::Label::new(Some(&source.name));
            title.add_css_class("heading");
            title.set_wrap(true);
            title.set_lines(2);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.set_max_width_chars(30);
            contents.append(&title);
            let detail = format!("{}  ·  {}", source.model, source.serial);
            let detail = gtk::Label::new(Some(&detail));
            detail.add_css_class("dim-label");
            detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
            detail.set_max_width_chars(30);
            contents.append(&detail);
            let profile = gtk::Label::new(Some(&source.selected_profile_label()));
            profile.set_ellipsize(gtk::pango::EllipsizeMode::End);
            profile.set_max_width_chars(30);
            contents.append(&profile);

            let open = gtk::Button::builder()
                .child(&contents)
                .hexpand(true)
                .build();
            open.add_css_class("card");
            let parent = self.window.clone();
            let camera = source.clone();
            let library = Rc::downgrade(self);
            open.connect_clicked(move |_| {
                if let (Some(parent), Some(library)) = (parent.upgrade(), library.upgrade()) {
                    let feed = feed::open(&parent, camera.clone());
                    let mut feeds = library.feeds.borrow_mut();
                    let windows = feeds.entry(camera.id).or_default();
                    windows.retain(|window| window.upgrade().is_some());
                    windows.push(feed.downgrade());
                }
            });
            let configure = gtk::Button::from_icon_name("emblem-system-symbolic");
            configure.set_tooltip_text(Some("Camera Settings"));
            configure.add_css_class("flat");
            configure.add_css_class("circular");
            let weak = Rc::downgrade(self);
            let camera = source.clone();
            configure.connect_clicked(move |_| {
                let Some(library) = weak.upgrade() else {
                    return;
                };
                let Some(parent) = library.window.upgrade() else {
                    return;
                };
                if library.busy.get() || !library.ready.get() {
                    library.toasts.add_toast(adw::Toast::new("Wait for the camera change to finish."));
                    return;
                }
                let save_library = Rc::clone(&library);
                let remove_library = Rc::clone(&library);
                settings::open(
                    &parent,
                    camera.clone(),
                    move |source| save_library.save_source(source),
                    move |id| remove_library.remove_source(id),
                );
            });
            let choices = gtk::Box::new(gtk::Orientation::Vertical, 3);
            choices.set_margin_top(6);
            choices.set_margin_bottom(6);
            choices.set_margin_start(6);
            choices.set_margin_end(6);
            for available in &source.profiles {
                let selected = source.selected_profile.as_deref() == Some(available.token.as_str());
                let label = if selected {
                    format!("✓  {}", Source::profile_label(available))
                } else {
                    format!("    {}", Source::profile_label(available))
                };
                let choice = gtk::Button::with_label(&label);
                choice.add_css_class("flat");
                let weak = Rc::downgrade(self);
                let camera = source.clone();
                let token = available.token.clone();
                choice.connect_clicked(move |_| {
                    if !selected {
                        if let Some(library) = weak.upgrade() {
                            let mut camera = camera.clone();
                            camera.selected_profile = Some(token.clone());
                            let rx = library.save_source(camera);
                            let weak = Rc::downgrade(&library);
                            await_store(rx, move |result| {
                                if let (Err(error), Some(library)) = (result, weak.upgrade()) {
                                    library.toasts.add_toast(adw::Toast::new(&error));
                                }
                            });
                        }
                    }
                });
                choices.append(&choice);
            }
            if source.profiles.is_empty() {
                choices.append(&gtk::Label::new(Some("No profiles available")));
            }
            let popover = gtk::Popover::new();
            popover.set_child(Some(&choices));
            let profiles = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .popover(&popover)
                .tooltip_text("Select Profile")
                .build();
            profiles.add_css_class("flat");
            profiles.add_css_class("circular");
            configure.set_halign(gtk::Align::Start);
            configure.set_valign(gtk::Align::End);
            configure.set_margin_start(8);
            configure.set_margin_bottom(8);
            profiles.set_halign(gtk::Align::End);
            profiles.set_valign(gtk::Align::End);
            profiles.set_margin_end(8);
            profiles.set_margin_bottom(8);
            let card = gtk::Overlay::new();
            let dimensions = source
                .selected_profile
                .as_ref()
                .and_then(|token| {
                    source
                        .profiles
                        .iter()
                        .find(|profile| &profile.token == token)
                })
                .and_then(|profile| profile.width.zip(profile.height))
                .filter(|(width, height)| *width > 0 && *height > 0);
            let ratio = dimensions
                .map(|(width, height)| (width as f64 / height as f64).clamp(0.0001, 10000.0))
                .unwrap_or(16.0 / 9.0);
            let height = (320.0_f64 / ratio).min(400.0);
            let width = height * ratio;
            let aspect = gtk::AspectFrame::new(0.5, 0.5, ratio as f32, false);
            aspect.set_size_request(
                (width.round() as i32).max(1),
                (height.round() as i32).max(1),
            );
            card.set_child(Some(&open));
            card.add_overlay(&configure);
            card.add_overlay(&profiles);
            aspect.set_child(Some(&card));
            self.grid.insert(&aspect, -1);
        }
    }

    fn load(self: &Rc<Self>, done: impl FnOnce(StoreResult<()>) + 'static) {
        if self.loading.replace(true) {
            done(Err("Camera library is already loading. Try again.".to_owned()));
            return;
        }
        self.ready.set(false);
        self.stack.set_visible_child_name("loading");
        self.empty_add.set_sensitive(false);
        if let Some(button) = self.header_add.upgrade() {
            button.set_sensitive(false);
            button.set_visible(false);
        }
        self.load_retry.set_sensitive(false);
        self.load_status.set_text("Loading camera library…");
        let (tx, rx) = mpsc::channel();
        if let Some(store) = &self.store {
            let _ = store.send(StoreRequest::Load(tx));
        }
        let library = Rc::clone(self);
        await_store(rx, move |result| {
            library.loading.set(false);
            match result {
                Ok(sources) => {
                    let loaded_next = sources.iter().map(|source| source.id).max()
                        .unwrap_or(0).checked_add(1);
                    library.next_id.set(match (library.next_id.get(), loaded_next) {
                        (Some(current), Some(loaded)) => Some(current.max(loaded)),
                        _ => None,
                    });
                    {
                        let previous = library.sources.borrow();
                        library.feeds.borrow_mut().retain(|id, windows| {
                            let Some(source) = sources.iter().find(|source| source.id == *id) else {
                                for window in windows.iter().filter_map(|window| window.upgrade()) {
                                    window.close();
                                }
                                return false;
                            };
                            if previous.iter().find(|old| old.id == *id)
                                .map_or(true, |old| !same_source(old, source))
                            {
                                windows.retain(|window| {
                                    if let Some(window) = window.upgrade() {
                                        feed::change_profile(&window, source.clone());
                                        true
                                    } else {
                                        false
                                    }
                                });
                            }
                            true
                        });
                    }
                    *library.sources.borrow_mut() = sources;
                    library.ready.set(true);
                    library.empty_add.set_sensitive(true);
                    if let Some(button) = library.header_add.upgrade() {
                        button.set_sensitive(true);
                    }
                    library.refresh();
                    done(Ok(()));
                }
                Err(error) => {
                    library.load_status.set_text(&error);
                    library.load_retry.set_sensitive(true);
                    done(Err(error));
                }
            }
        });
    }

    fn mutate(
        self: &Rc<Self>,
        make_request: impl FnOnce(mpsc::Sender<StoreResult<()>>) -> StoreRequest,
        success: impl FnOnce(&Rc<Self>) + 'static,
    ) -> mpsc::Receiver<StoreResult<()>> {
        if !self.ready.get() {
            return failed_operation("Camera library is unavailable. Retry loading it; if a dialog is open, close it first.");
        }
        if self.busy.replace(true) {
            return failed_operation("Wait for the camera change to finish, then try again.");
        }
        let (tx, rx) = mpsc::channel();
        if let Some(store) = &self.store {
            let _ = store.send(make_request(tx));
        }
        let (done, result) = mpsc::channel();
        let library = Rc::clone(self);
        await_store(rx, move |outcome| {
            match outcome {
                Ok(()) => {
                    success(&library);
                    library.busy.set(false);
                    let _ = done.send(Ok(()));
                }
                Err(error) => {
                    let after_load = Rc::clone(&library);
                    library.load(move |reload| {
                        after_load.busy.set(false);
                        let message = match reload {
                            Ok(()) => format!("{error}\n\nThe library was reloaded; check the camera before retrying."),
                            Err(reload_error) => format!("{error}\n\nCould not reload the library: {reload_error}\nRetry loading it; if a dialog is open, close it first."),
                        };
                        let _ = done.send(Err(message));
                    });
                }
            }
        });
        result
    }

    fn insert_source(self: &Rc<Self>, source: Source) -> mpsc::Receiver<StoreResult<()>> {
        let saved = source.clone();
        self.mutate(move |tx| StoreRequest::Insert(source, tx), move |library| {
            library.sources.borrow_mut().push(saved);
            library.refresh();
        })
    }

    fn save_source(self: &Rc<Self>, source: Source) -> mpsc::Receiver<StoreResult<()>> {
        if !self.sources.borrow().iter().any(|item| item.id == source.id) {
            return failed_operation("This camera is no longer in the library. Close settings and check the library.");
        }
        let saved = source.clone();
        self.mutate(move |tx| StoreRequest::Update(source, tx), move |library| {
            if let Some(current) = library.sources.borrow_mut().iter_mut().find(|item| item.id == saved.id) {
                *current = saved.clone();
            }
            if let Some(windows) = library.feeds.borrow_mut().get_mut(&saved.id) {
                windows.retain(|window| {
                    if let Some(window) = window.upgrade() {
                        feed::change_profile(&window, saved.clone());
                        true
                    } else {
                        false
                    }
                });
            }
            library.refresh();
        })
    }

    fn remove_source(self: &Rc<Self>, id: u64) -> mpsc::Receiver<StoreResult<()>> {
        if !self.sources.borrow().iter().any(|item| item.id == id) {
            return failed_operation("This camera is no longer in the library. Close settings and check the library.");
        }
        self.mutate(move |tx| StoreRequest::Remove(id, tx), move |library| {
            library.sources.borrow_mut().retain(|source| source.id != id);
            if let Some(windows) = library.feeds.borrow_mut().remove(&id) {
                for window in windows {
                    if let Some(window) = window.upgrade() {
                        window.close();
                    }
                }
            }
            library.refresh();
        })
    }
}

struct AddState {
    window: adw::Window,
    name: adw::EntryRow,
    address: adw::EntryRow,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    add: gtk::Button,
    status: gtk::Label,
    spinner: gtk::Spinner,
    cancel: RefCell<Option<watch::Sender<bool>>>,
    reserved_id: Cell<Option<u64>>,
    closed: Cell<bool>,
}

impl AddState {
    fn show_error(&self, message: &str) {
        self.cancel.borrow_mut().take();
        self.add.set_sensitive(true);
        self.address.set_sensitive(true);
        self.username.set_sensitive(true);
        self.password.set_sensitive(true);
        self.name.set_sensitive(true);
        self.spinner.stop();
        self.status.add_css_class("error");
        self.status.set_text(message);
    }
}

fn add_camera(library: &Rc<Library>) {
    let Some(parent) = library.window.upgrade() else {
        return;
    };
    let name = adw::EntryRow::builder().title("Name (Optional)").build();
    let address = adw::EntryRow::builder()
        .title("Device Service Address")
        .build();
    let username = adw::EntryRow::builder().title("Username").build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
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
    status.add_css_class("error");
    let spinner = gtk::Spinner::new();
    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_row.append(&spinner);
    status_row.append(&status);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    body.set_margin_top(18);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.append(&group);
    body.append(&status_row);
    let add = gtk::Button::with_label("Add");
    add.add_css_class("suggested-action");
    let header = adw::HeaderBar::new();
    header.pack_end(&add);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));
    let window = adw::Window::builder()
        .title("Add Camera")
        .transient_for(&parent)
        .destroy_with_parent(true)
        .modal(true)
        .default_width(440)
        .content(&toolbar)
        .build();
    let ui = Rc::new(AddState {
        window,
        name,
        address,
        username,
        password,
        add,
        status,
        spinner,
        cancel: RefCell::new(None),
        reserved_id: Cell::new(None),
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
    let library = Rc::clone(library);
    ui.add.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        let address = ui.address.text().trim().to_owned();
        if address.is_empty() {
            ui.status.set_text("Enter the camera’s device service address.");
            return;
        }
        let name = ui.name.text().trim().to_owned();
        let username = ui.username.text().to_string();
        let password = ui.password.text().to_string();
        let (cancel, mut cancel_rx) = watch::channel(false);
        let (tx, rx) = mpsc::channel();
        let worker_address = address.clone();
        let worker_username = username.clone();
        let worker_password = password.clone();
        if std::thread::Builder::new().name("onvif-add-camera".into()).spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(async {
                    tokio::select! {
                        _ = cancel_rx.changed() => Err("Camera connection cancelled.".to_owned()),
                        result = onvif::connect(&worker_address, &worker_username, &worker_password) => {
                            result.map(|camera| {
                                let information = camera.information;
                                let device_name = format!("{} {}", information.manufacturer, information.model).trim().to_owned();
                                (device_name, information.model, information.serial_number, camera.profiles)
                            }).map_err(|error| settings::connection_error(&error).to_owned())
                        }
                    }
                }),
                Err(_) => Err("Could not initialize the camera connection.".to_owned()),
            };
            let _ = tx.send(result);
        }).is_err() {
            ui.status.set_text("Could not start the camera connection worker.");
            return;
        }
        *ui.cancel.borrow_mut() = Some(cancel);
        ui.add.set_sensitive(false);
        ui.address.set_sensitive(false);
        ui.username.set_sensitive(false);
        ui.password.set_sensitive(false);
        ui.name.set_sensitive(false);
        ui.status.remove_css_class("error");
        ui.status.set_text("Connecting to camera…");
        ui.spinner.start();
        let weak_ui = Rc::downgrade(&ui);
        let library = Rc::clone(&library);
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let Some(ui) = weak_ui.upgrade() else { return glib::ControlFlow::Break };
            if ui.closed.get() { return glib::ControlFlow::Break; }
            match rx.try_recv() {
                Ok(Ok((device_name, model, serial, profiles))) => {
                    let id = if let Some(id) = ui.reserved_id.get() {
                        id
                    } else if let Some(id) = library.next_id.get() {
                        library.next_id.set(id.checked_add(1));
                        ui.reserved_id.set(Some(id));
                        id
                    } else {
                        ui.show_error("No more camera IDs are available.");
                        return glib::ControlFlow::Break;
                    };
                    ui.cancel.borrow_mut().take();
                    ui.status.set_text("Saving camera library…");
                    let source = Source {
                        id,
                        name: if name.is_empty() {
                            if device_name.is_empty() { "Camera".to_owned() } else { device_name }
                        } else { name.clone() },
                        address: address.clone(),
                        username: username.clone(),
                        password: password.clone(),
                        model: model.clone(),
                        serial: serial.clone(),
                        selected_profile: Source::default_profile(&profiles),
                        profiles: profiles.clone(),
                    };
                    let exists = library.sources.borrow().iter().any(|item| item.id == id);
                    let result = if exists {
                        library.save_source(source)
                    } else {
                        library.insert_source(source)
                    };
                    let weak_ui = Rc::downgrade(&ui);
                    await_store(result, move |outcome| {
                        let Some(ui) = weak_ui.upgrade() else { return };
                        if ui.closed.get() { return; }
                        match outcome {
                            Ok(()) => ui.window.close(),
                            Err(error) => ui.show_error(&error),
                        }
                    });
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    ui.show_error(&error);
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
    // The timeout owns the dialog while it is open; closing always stops it.
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

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let empty = adw::StatusPage::builder()
        .icon_name("camera-web-symbolic")
        .title("No Cameras")
        .description("Add a camera to start viewing its video.")
        .build();
    let empty_add = gtk::Button::with_label("Add Camera");
    empty_add.add_css_class("suggested-action");
    empty_add.add_css_class("pill");
    empty_add.set_halign(gtk::Align::Center);
    empty.set_child(Some(&empty_add));
    empty_add.set_sensitive(false);

    let load_status = gtk::Label::new(Some("Loading camera library…"));
    load_status.set_wrap(true);
    load_status.set_max_width_chars(65);
    let load_retry = gtk::Button::with_label("Retry");
    load_retry.set_halign(gtk::Align::Center);
    let load_body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    load_body.set_valign(gtk::Align::Center);
    load_body.set_halign(gtk::Align::Center);
    load_body.set_margin_start(24);
    load_body.set_margin_end(24);
    load_body.append(&load_status);
    load_body.append(&load_retry);

    let grid = gtk::FlowBox::new();
    grid.set_selection_mode(gtk::SelectionMode::None);
    grid.set_max_children_per_line(4);
    grid.set_column_spacing(18);
    grid.set_row_spacing(18);
    grid.set_margin_top(24);
    grid.set_margin_bottom(24);
    grid.set_margin_start(24);
    grid.set_margin_end(24);
    grid.set_valign(gtk::Align::Start);
    let scroll = gtk::ScrolledWindow::builder().child(&grid).build();
    let stack = gtk::Stack::new();
    stack.add_named(&load_body, Some("loading"));
    stack.set_visible_child_name("loading");
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&scroll, Some("sources"));
    let add = gtk::Button::with_label("Add Camera");
    add.set_visible(false);
    add.set_sensitive(false);
    let header = adw::HeaderBar::new();
    header.pack_start(&add);
    let menu = gtk::gio::Menu::new();
    menu.append(Some("About ONVIF Viewer"), Some("win.about"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main Menu")
        .build();
    header.pack_end(&menu_button);
    let layout = adw::ToolbarView::new();
    layout.add_top_bar(&header);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    layout.set_content(Some(&toasts));
    let window = ApplicationWindow::builder()
        .application(app)
        .title("ONVIF Viewer")
        .default_width(900)
        .default_height(600)
        .content(&layout)
        .build();
    let about = gtk::gio::SimpleAction::new("about", None);
    let parent = window.downgrade();
    about.connect_activate(move |_, _| {
        if let Some(parent) = parent.upgrade() {
            let dialog = adw::AboutWindow::builder()
                .transient_for(&parent)
                .application_name("ONVIF Viewer")
                .application_icon("camera-web-symbolic")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("View ONVIF camera streams.")
                .build();
            dialog.present();
        }
    });
    window.add_action(&about);
    let library = Rc::new(Library {
        window: window.downgrade(),
        stack,
        grid,
        toasts,
        header_add: add.downgrade(),
        store: start_store_worker(),
        sources: RefCell::new(Vec::new()),
        busy: Cell::new(false),
        ready: Cell::new(false),
        feeds: RefCell::new(HashMap::new()),
        next_id: Cell::new(Some(1)),
        loading: Cell::new(false),
        load_status,
        load_retry: load_retry.clone(),
        empty_add: empty_add.clone(),
    });
    let strong = Rc::clone(&library);
    add.connect_clicked(move |_| add_camera(&strong));
    let weak = Rc::downgrade(&library);
    empty_add.connect_clicked(move |_| {
        if let Some(library) = weak.upgrade() {
            add_camera(&library);
        }
    });
    let weak = Rc::downgrade(&library);
    load_retry.connect_clicked(move |_| {
        if let Some(library) = weak.upgrade() {
            library.load(|_| {});
        }
    });
    library.load(|_| {});
    window.present();
}
