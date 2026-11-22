//! GTK LAN viewer settings dialog.
//!
//! `open` loads and saves on worker threads. Its callback is invoked on the GTK
//! thread after persistence succeeds; it must apply the configuration and return
//! a receiver that reports success or an actionable server error. A failed apply
//! leaves the saved configuration intact and the dialog open for correction.

use crate::web_config::{self, Config};
use adw::prelude::*;
use adw::{glib, gtk};
use std::{cell::Cell, rc::Rc, sync::mpsc, time::Duration};

type ApplyResult = Result<(), String>;

fn wait_for<T: 'static>(
    rx: mpsc::Receiver<Result<T, String>>,
    callback: impl FnOnce(Result<T, String>) + 'static,
) {
    let mut callback = Some(callback);
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("The LAN settings worker stopped. Try again.".to_owned())
            }
        };
        callback.take().unwrap()(result);
        glib::ControlFlow::Break
    });
}

fn work<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> mpsc::Receiver<Result<T, String>> {
    let (tx, rx) = mpsc::channel();
    let failure = tx.clone();
    if std::thread::Builder::new()
        .name("lan-settings".into())
        .spawn(move || {
            let _ = tx.send(task());
        })
        .is_err()
    {
        let _ = failure.send(Err(
            "Could not start the LAN settings worker. Try again.".to_owned()
        ));
    }
    rx
}

struct Form {
    window: adw::Window,
    enabled: gtk::Switch,
    port: adw::EntryRow,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    save: gtk::Button,
    retry: gtk::Button,
    status: gtk::Label,
    fields: adw::PreferencesGroup,
}

impl Form {
    fn message(&self, message: &str) {
        self.status.set_text(message);
    }
    fn load(self: &Rc<Self>) {
        self.fields.set_sensitive(false);
        self.save.set_sensitive(false);
        self.retry.set_sensitive(false);
        self.message("Loading LAN viewer settings…");
        let ui = Rc::clone(self);
        wait_for(
            work(|| web_config::load().map_err(|e| e.to_string())),
            move |result| {
                ui.retry.set_sensitive(true);
                match result {
                    Ok(config) => {
                        ui.enabled.set_active(config.enabled);
                        ui.port.set_text(&config.port.to_string());
                        ui.username.set_text(&config.username);
                        ui.password.set_text(&config.password);
                        ui.fields.set_sensitive(true);
                        ui.save.set_sensitive(true);
                        ui.retry.set_visible(false);
                        ui.message("");
                    }
                    Err(error) => {
                        ui.retry.set_visible(true);
                        ui.message(&error);
                    }
                }
            },
        );
    }
}

/// Open the settings window. `on_apply` runs after each successful save and
/// should start/reconfigure/stop the server, returning a receiver for its result.
/// Callers should separately load settings at startup on a worker thread and
/// show errors without terminating the GTK application.
pub fn open(
    parent: &adw::ApplicationWindow,
    on_apply: impl Fn(Config) -> mpsc::Receiver<ApplyResult> + 'static,
) {
    let enabled = gtk::Switch::new();
    enabled.set_valign(gtk::Align::Center);
    let enabled_row = adw::ActionRow::builder().title("Enable LAN viewer").build();
    enabled_row.add_suffix(&enabled);
    enabled_row.set_activatable_widget(Some(&enabled));
    let port = adw::EntryRow::builder().title("Port (1–65535)").build();
    let username = adw::EntryRow::builder().title("Username").build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    let fields = adw::PreferencesGroup::new();
    fields.add(&enabled_row);
    fields.add(&port);
    fields.add(&username);
    fields.add(&password);
    let warning = gtk::Label::new(Some(
        "Security warning: HTTP is not encrypted. Your username, password, and camera video can be observed on the network. Use only on a trusted LAN; do not expose this server to the internet.",
    ));
    warning.set_wrap(true);
    warning.set_xalign(0.0);
    warning.add_css_class("warning");
    let address = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        })
        .ok()
        .map(|address| address.ip().to_string());
    let info = gtk::Label::new(None);
    let update_info = {
        let info = info.clone();
        move |port: &adw::EntryRow| {
            let address = address
                .as_deref()
                .unwrap_or("<this computer’s LAN IP address>");
            info.set_text(&format!("On the same network, try http://{address}:{}/. The address follows the default route; other interfaces may use a different address. Allow the port through your firewall if needed.", port.text()));
        }
    };
    port.connect_changed(move |port| update_info(port));
    info.set_wrap(true);
    info.set_xalign(0.0);
    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(0.0);
    let retry = gtk::Button::with_label("Retry loading");
    retry.set_visible(false);
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    body.set_margin_top(18);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.append(&warning);
    body.append(&info);
    body.append(&fields);
    body.append(&status);
    body.append(&retry);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&body)
        .build();
    let header = adw::HeaderBar::new();
    header.pack_end(&save);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));
    let window = adw::Window::builder()
        .title("LAN Web Viewer")
        .transient_for(parent)
        .destroy_with_parent(true)
        .modal(true)
        .default_width(500)
        .default_height(550)
        .content(&toolbar)
        .build();
    let ui = Rc::new(Form {
        window,
        enabled,
        port,
        username,
        password,
        save,
        retry,
        status,
        fields,
    });
    let weak = Rc::downgrade(&ui);
    ui.retry.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.load();
        }
    });
    let weak = Rc::downgrade(&ui);
    let on_apply = Rc::new(on_apply);
    ui.save.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        let port = match ui.port.text().trim().parse::<u16>() {
            Ok(port) if port != 0 => port,
            _ => {
                ui.message("Enter a port between 1 and 65535.");
                return;
            }
        };
        let config = Config {
            enabled: ui.enabled.is_active(),
            port,
            username: ui.username.text().to_string(),
            password: ui.password.text().to_string(),
        };
        if let Err(error) = web_config::validate(&config) {
            ui.message(&error.to_string());
            return;
        }
        ui.fields.set_sensitive(false);
        ui.save.set_sensitive(false);
        ui.message("Saving LAN viewer settings…");
        let to_save = config.clone();
        let on_apply = Rc::clone(&on_apply);
        wait_for(
            work(move || web_config::save(&to_save).map_err(|e| e.to_string())),
            move |result| match result {
                Err(error) => {
                    ui.fields.set_sensitive(true);
                    ui.save.set_sensitive(true);
                    ui.message(&error);
                }
                Ok(()) => {
                    ui.message("Applying LAN viewer settings…");
                    wait_for(on_apply(config), move |result| {
                        ui.fields.set_sensitive(true);
                        ui.save.set_sensitive(true);
                        match result {
                            Ok(()) => ui.window.close(),
                            Err(error) => ui.message(&format!(
                                "Settings saved, but the server could not be applied: {error}"
                            )),
                        }
                    });
                }
            },
        );
    });
    // The timer owns the form until the window's destroy signal fires.
    let closed = Rc::new(Cell::new(false));
    let destroyed = Rc::clone(&closed);
    ui.window.connect_destroy(move |_| destroyed.set(true));
    let keep_alive = Rc::clone(&ui);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if closed.get() {
            glib::ControlFlow::Break
        } else {
            let _ = &keep_alive;
            glib::ControlFlow::Continue
        }
    });
    ui.load();
    ui.window.present();
}
