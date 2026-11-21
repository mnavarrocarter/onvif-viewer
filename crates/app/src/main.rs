mod feed;
mod grid_playback;
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
const GRID_MARGIN: i32 = 24;
const GRID_SPACING: i32 = 18;
// FlowBoxChild adds padding around each aspect frame.
const GRID_CHILD_PADDING: f64 = 12.0;
const MIN_TILE_WIDTH: f64 = 320.0;

fn grid_layout(width: i32, height: i32, count: usize, min_ratio: f64) -> (u32, i32) {
    let width = (width - 2 * GRID_MARGIN).max(1) as f64;
    let height = (height - 2 * GRID_MARGIN).max(1) as f64;
    let max_columns = count.clamp(1, 4);
    let ratio = min_ratio.clamp(0.0001, 10000.0);
    let mut best = None;
    for columns in 1..=max_columns {
        let rows = count.max(1).div_ceil(columns);
        let horizontal_gaps = (columns - 1) as f64 * GRID_SPACING as f64;
        let vertical_gaps = (rows - 1) as f64 * GRID_SPACING as f64;
        let cell_width = (width - horizontal_gaps) / columns as f64 - GRID_CHILD_PADDING;
        let cell_height = (height - vertical_gaps) / rows as f64 - GRID_CHILD_PADDING;
        let tile_width = cell_width.min(cell_height * ratio);
        if tile_width >= MIN_TILE_WIDTH && best.is_none_or(|(_, _, size)| tile_width > size) {
            let grid_width = ((tile_width + GRID_CHILD_PADDING) * columns as f64 + horizontal_gaps)
                .ceil() as i32;
            best = Some((columns as u32, grid_width, tile_width));
        }
    }
    if let Some((columns, grid_width, _)) = best {
        (columns, grid_width)
    } else {
        // Keep controls usable and allow scrolling when no arrangement fits.
        let columns = ((width + GRID_SPACING as f64)
            / (MIN_TILE_WIDTH + GRID_CHILD_PADDING + GRID_SPACING as f64))
            .floor()
            .max(1.0) as usize;
        (columns.min(max_columns) as u32, width as i32)
    }
}

fn source_ratio(source: &Source) -> f32 {
    grid_playback::lowest_resolution_profile(&source.profiles)
        .and_then(|profile| profile.width.zip(profile.height))
        .filter(|(width, height)| *width > 0 && *height > 0)
        .map_or(16.0 / 9.0, |(width, height)| {
            (width as f32 / height as f32).clamp(0.0001, 10000.0)
        })
}

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
                        let result = store
                            .as_mut()
                            .ok_or_else(|| {
                                "Camera library is not available. Retry loading it.".to_owned()
                            })
                            .and_then(|store| store.insert(source).map_err(|e| e.to_string()));
                        let _ = reply.send(result);
                    }
                    StoreRequest::Update(source, reply) => {
                        let result = store
                            .as_mut()
                            .ok_or_else(|| {
                                "Camera library is not available. Retry loading it.".to_owned()
                            })
                            .and_then(|store| store.update(source).map_err(|e| e.to_string()));
                        let _ = reply.send(result);
                    }
                    StoreRequest::Remove(id, reply) => {
                        let result = store
                            .as_mut()
                            .ok_or_else(|| {
                                "Camera library is not available. Retry loading it.".to_owned()
                            })
                            .and_then(|store| store.remove(id).map_err(|e| e.to_string()));
                        let _ = reply.send(result);
                    }
                }
            }
        })
        .ok()?;
    Some(tx)
}

fn await_store<T: 'static>(
    rx: mpsc::Receiver<StoreResult<T>>,
    callback: impl FnOnce(StoreResult<T>) + 'static,
) {
    let mut callback = Some(callback);
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => Err(
                "Camera library worker stopped. Restart the application and try again.".to_owned(),
            ),
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
            a.token == b.token
                && a.name == b.name
                && a.width == b.width
                && a.height == b.height
                && a.codec == b.codec
                && a.audio_codec == b.audio_codec
        })
}

struct Tile {
    source: RefCell<Source>,
    playback: Rc<grid_playback::GridPlayback>,
    widget: gtk::AspectFrame,
    title: gtk::Label,
    choices: gtk::Box,
    play: gtk::Button,
    open_feeds: Cell<usize>,
    resume: Cell<bool>,
}

impl Tile {
    fn update_play_button(&self) {
        let playing = self.playback.is_playing();
        self.play.set_icon_name(if playing {
            "media-playback-stop-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
        self.play
            .set_tooltip_text(Some(if playing { "Stop" } else { "Play" }));
    }

    fn watch_playback(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(250), move || {
            let Some(tile) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            tile.update_play_button();
            if tile.playback.is_playing() {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }
}

struct Library {
    window: glib::WeakRef<ApplicationWindow>,
    stack: gtk::Stack,
    grid: gtk::FlowBox,
    grid_wrapper: gtk::Box,
    scroll: gtk::ScrolledWindow,
    layout_pending: Cell<bool>,
    layout_size: Cell<(u32, i32)>,
    toasts: adw::ToastOverlay,
    header_add: glib::WeakRef<gtk::Button>,
    store: Option<mpsc::Sender<StoreRequest>>,
    sources: RefCell<Vec<Source>>,
    busy: Cell<bool>,
    ready: Cell<bool>,
    feeds: RefCell<HashMap<u64, Vec<glib::WeakRef<adw::Window>>>>,
    tiles: RefCell<HashMap<u64, Rc<Tile>>>,
    shutting_down: Cell<bool>,
    next_id: Cell<Option<u64>>,
    loading: Cell<bool>,
    load_status: gtk::Label,
    load_retry: gtk::Button,
    empty_add: gtk::Button,
}

impl Library {
    fn queue_grid_layout(self: &Rc<Self>) {
        if self.layout_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(library) = weak.upgrade() else {
                return;
            };
            library.layout_pending.set(false);
            let (width, height) = (library.scroll.width(), library.scroll.height());
            if width <= 1 || height <= 1 {
                return;
            }
            let sources = library.sources.borrow();
            let ratio = sources
                .iter()
                .map(|source| source_ratio(source) as f64)
                .fold(f64::INFINITY, f64::min);
            let layout = grid_layout(width, height, sources.len(), ratio);
            if library.layout_size.replace(layout) != layout {
                library.grid.set_max_children_per_line(layout.0);
                library
                    .grid_wrapper
                    .set_size_request(layout.1 + 2 * GRID_MARGIN, -1);
            }
        });
    }

    fn refresh(self: &Rc<Self>) {
        let sources = self.sources.borrow().clone();
        self.queue_grid_layout();
        {
            let mut tiles = self.tiles.borrow_mut();
            tiles.retain(|id, tile| {
                if sources.iter().any(|source| source.id == *id) {
                    true
                } else {
                    tile.resume.set(false);
                    tile.playback.close();
                    if let Some(child) = tile.widget.parent() {
                        self.grid.remove(&child);
                    }
                    false
                }
            });
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
            let existing = self.tiles.borrow().get(&source.id).cloned();
            let tile = if let Some(tile) = existing {
                if !same_source(&tile.source.borrow(), &source) {
                    tile.playback.update_source(source.clone());
                    *tile.source.borrow_mut() = source.clone();
                    self.update_tile(&tile, &source);
                }
                tile
            } else {
                let tile = self.create_tile(&source);
                self.update_tile(&tile, &source);
                self.grid.insert(&tile.widget, -1);
                self.tiles.borrow_mut().insert(source.id, Rc::clone(&tile));
                tile
            };
            tile.update_play_button();
        }
    }

    fn create_tile(self: &Rc<Self>, source: &Source) -> Rc<Tile> {
        let playback = grid_playback::GridPlayback::new(source.clone());
        playback.set_muted(true);
        playback.set_volume(1.0);
        let video = playback.widget();
        video.set_hexpand(true);
        video.set_vexpand(true);
        let card = gtk::Overlay::new();
        card.set_child(Some(&video));
        let title = gtk::Label::new(Some(&source.name));
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_halign(gtk::Align::Start);
        title.set_valign(gtk::Align::Start);
        title.set_margin_top(8);
        title.set_margin_start(8);
        title.add_css_class("grid-video-label");
        card.add_overlay(&title);

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        controls.add_css_class("grid-controls");
        controls.set_halign(gtk::Align::Fill);
        controls.set_valign(gtk::Align::End);
        controls.set_margin_start(8);
        controls.set_margin_end(8);
        controls.set_margin_bottom(8);
        let configure = gtk::Button::from_icon_name("emblem-system-symbolic");
        configure.set_tooltip_text(Some("Camera Settings"));
        configure.add_css_class("flat");
        configure.add_css_class("circular");
        controls.append(&configure);
        let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
        play.add_css_class("flat");
        play.add_css_class("circular");
        controls.append(&play);
        let mute = gtk::ToggleButton::new();
        mute.set_icon_name("audio-volume-muted-symbolic");
        mute.set_tooltip_text(Some("Unmute"));
        mute.set_active(true);
        mute.add_css_class("flat");
        mute.add_css_class("circular");
        controls.append(&mute);
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        volume.set_value(1.0);
        volume.set_draw_value(false);
        volume.set_size_request(70, -1);
        volume.set_hexpand(true);
        volume.set_tooltip_text(Some("Volume"));
        controls.append(&volume);
        controls.append(playback.snapshot_button());
        controls.append(playback.record_button());
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 3);
        choices.set_margin_top(6);
        choices.set_margin_bottom(6);
        choices.set_margin_start(6);
        choices.set_margin_end(6);
        let popover = gtk::Popover::new();
        popover.set_child(Some(&choices));
        let profiles = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .popover(&popover)
            .tooltip_text("Select Profile")
            .build();
        profiles.add_css_class("flat");
        profiles.add_css_class("circular");
        controls.append(&profiles);
        card.add_overlay(&controls);
        let aspect = gtk::AspectFrame::new(0.5, 0.5, 16.0 / 9.0, false);
        aspect.set_size_request(320, 180);
        aspect.set_child(Some(&card));
        let tile = Rc::new(Tile {
            source: RefCell::new(source.clone()),
            playback,
            widget: aspect,
            title,
            choices,
            play: play.clone(),
            open_feeds: Cell::new(0),
            resume: Cell::new(false),
        });

        let weak_tile = Rc::downgrade(&tile);
        play.connect_clicked(move |_| {
            if let Some(tile) = weak_tile.upgrade() {
                if tile.playback.is_playing() {
                    tile.playback.stop();
                } else {
                    tile.playback.play();
                    tile.watch_playback();
                }
                tile.update_play_button();
            }
        });
        let playback = Rc::clone(&tile.playback);
        mute.connect_toggled(move |button| {
            let muted = button.is_active();
            playback.set_muted(muted);
            button.set_icon_name(if muted {
                "audio-volume-muted-symbolic"
            } else {
                "audio-volume-high-symbolic"
            });
            button.set_tooltip_text(Some(if muted { "Unmute" } else { "Mute" }));
        });
        let playback = Rc::clone(&tile.playback);
        volume.connect_value_changed(move |volume| playback.set_volume(volume.value()));
        let weak = Rc::downgrade(self);
        let weak_tile = Rc::downgrade(&tile);
        configure.connect_clicked(move |_| {
            let (Some(library), Some(tile)) = (weak.upgrade(), weak_tile.upgrade()) else {
                return;
            };
            let Some(parent) = library.window.upgrade() else {
                return;
            };
            if library.busy.get() || !library.ready.get() {
                library
                    .toasts
                    .add_toast(adw::Toast::new("Wait for the camera change to finish."));
                return;
            }
            let save_library = Rc::clone(&library);
            let remove_library = Rc::clone(&library);
            settings::open(
                &parent,
                tile.source.borrow().clone(),
                move |source| save_library.save_source(source),
                move |id| remove_library.remove_source(id),
            );
        });
        let gesture = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        let weak_tile = Rc::downgrade(&tile);
        gesture.connect_released(move |_, _, _, _| {
            let (Some(library), Some(tile)) = (weak.upgrade(), weak_tile.upgrade()) else {
                return;
            };
            library.open_feed(&tile);
        });
        video.add_controller(gesture);
        tile
    }

    fn update_tile(self: &Rc<Self>, tile: &Rc<Tile>, source: &Source) {
        tile.widget.set_ratio(source_ratio(source));
        tile.title.set_text(&source.name);
        while let Some(child) = tile.choices.first_child() {
            tile.choices.remove(&child);
        }
        for available in &source.profiles {
            let selected = source.selected_profile.as_deref() == Some(available.token.as_str());
            let label = format!(
                "{}  {}",
                if selected { "✓" } else { " " },
                Source::profile_label(available)
            );
            let choice = gtk::Button::with_label(&label);
            choice.add_css_class("flat");
            let weak = Rc::downgrade(self);
            let weak_tile = Rc::downgrade(tile);
            let token = available.token.clone();
            choice.connect_clicked(move |_| {
                if selected {
                    return;
                }
                let (Some(library), Some(tile)) = (weak.upgrade(), weak_tile.upgrade()) else {
                    return;
                };
                let mut camera = tile.source.borrow().clone();
                camera.selected_profile = Some(token.clone());
                let rx = library.save_source(camera);
                let weak = Rc::downgrade(&library);
                await_store(rx, move |result| {
                    if let (Err(error), Some(library)) = (result, weak.upgrade()) {
                        library.toasts.add_toast(adw::Toast::new(&error));
                    }
                });
            });
            tile.choices.append(&choice);
        }
        if source.profiles.is_empty() {
            tile.choices
                .append(&gtk::Label::new(Some("No profiles available")));
        }
    }

    fn open_feed(self: &Rc<Self>, tile: &Rc<Tile>) {
        let Some(parent) = self.window.upgrade() else {
            return;
        };
        if self.shutting_down.get() {
            return;
        }
        if tile.open_feeds.get() == 0 {
            tile.resume.set(tile.playback.is_playing());
            tile.playback.stop();
            tile.update_play_button();
            tile.play.set_sensitive(false);
        }
        tile.open_feeds.set(tile.open_feeds.get() + 1);
        let camera = tile.source.borrow().clone();
        let weak = Rc::downgrade(self);
        let id = camera.id;
        let feed = feed::open(&parent, camera.clone(), move |token: String| {
            let Some(library) = weak.upgrade() else {
                return;
            };
            let Some(mut source) = library
                .sources
                .borrow()
                .iter()
                .find(|source| source.id == id)
                .cloned()
            else {
                library
                    .toasts
                    .add_toast(adw::Toast::new("This camera is no longer in the library."));
                return;
            };
            source.selected_profile = Some(token);
            let rx = library.save_source(source);
            let weak = Rc::downgrade(&library);
            await_store(rx, move |result| {
                if let (Err(error), Some(library)) = (result, weak.upgrade()) {
                    library.toasts.add_toast(adw::Toast::new(&error));
                }
            });
        });
        let id = camera.id;
        let weak = Rc::downgrade(self);
        let weak_tile = Rc::downgrade(tile);
        let closed = Cell::new(false);
        let restore = Rc::new(move || {
            if closed.replace(true) {
                return;
            }
            if let (Some(library), Some(tile)) = (weak.upgrade(), weak_tile.upgrade()) {
                let remaining = tile.open_feeds.get().saturating_sub(1);
                tile.open_feeds.set(remaining);
                if remaining == 0 {
                    tile.play.set_sensitive(true);
                    let present = library
                        .tiles
                        .borrow()
                        .get(&id)
                        .is_some_and(|current| Rc::ptr_eq(current, &tile));
                    if tile.resume.replace(false) && present && !library.shutting_down.get() {
                        tile.playback.play();
                        tile.watch_playback();
                    }
                    tile.update_play_button();
                }
            }
        });
        let on_close = Rc::clone(&restore);
        feed.connect_close_request(move |_| {
            on_close();
            glib::Propagation::Proceed
        });
        feed.connect_hide(move |_| restore());
        let mut feeds = self.feeds.borrow_mut();
        let windows = feeds.entry(camera.id).or_default();
        windows.retain(|window| window.upgrade().is_some());
        windows.push(feed.downgrade());
    }

    fn load(self: &Rc<Self>, done: impl FnOnce(StoreResult<()>) + 'static) {
        if self.loading.replace(true) {
            done(Err(
                "Camera library is already loading. Try again.".to_owned()
            ));
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
                    let loaded_next = sources
                        .iter()
                        .map(|source| source.id)
                        .max()
                        .unwrap_or(0)
                        .checked_add(1);
                    library
                        .next_id
                        .set(match (library.next_id.get(), loaded_next) {
                            (Some(current), Some(loaded)) => Some(current.max(loaded)),
                            _ => None,
                        });
                    {
                        let previous = library.sources.borrow();
                        library.feeds.borrow_mut().retain(|id, windows| {
                            let Some(source) = sources.iter().find(|source| source.id == *id)
                            else {
                                if let Some(tile) = library.tiles.borrow().get(id) {
                                    tile.resume.set(false);
                                }
                                for window in windows.iter().filter_map(|window| window.upgrade()) {
                                    window.close();
                                }
                                return false;
                            };
                            if previous
                                .iter()
                                .find(|old| old.id == *id)
                                .is_none_or(|old| !same_source(old, source))
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
            return failed_operation(
                "Camera library is unavailable. Retry loading it; if a dialog is open, close it first.",
            );
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
        await_store(rx, move |outcome| match outcome {
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
        });
        result
    }

    fn insert_source(self: &Rc<Self>, source: Source) -> mpsc::Receiver<StoreResult<()>> {
        let saved = source.clone();
        self.mutate(
            move |tx| StoreRequest::Insert(source, tx),
            move |library| {
                library.sources.borrow_mut().push(saved);
                library.refresh();
            },
        )
    }

    fn save_source(self: &Rc<Self>, source: Source) -> mpsc::Receiver<StoreResult<()>> {
        if !self
            .sources
            .borrow()
            .iter()
            .any(|item| item.id == source.id)
        {
            return failed_operation(
                "This camera is no longer in the library. Close settings and check the library.",
            );
        }
        let saved = source.clone();
        self.mutate(
            move |tx| StoreRequest::Update(source, tx),
            move |library| {
                if let Some(current) = library
                    .sources
                    .borrow_mut()
                    .iter_mut()
                    .find(|item| item.id == saved.id)
                {
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
            },
        )
    }

    fn remove_source(self: &Rc<Self>, id: u64) -> mpsc::Receiver<StoreResult<()>> {
        if !self.sources.borrow().iter().any(|item| item.id == id) {
            return failed_operation(
                "This camera is no longer in the library. Close settings and check the library.",
            );
        }
        self.mutate(
            move |tx| StoreRequest::Remove(id, tx),
            move |library| {
                library
                    .sources
                    .borrow_mut()
                    .retain(|source| source.id != id);
                if let Some(tile) = library.tiles.borrow().get(&id) {
                    tile.resume.set(false);
                }
                if let Some(windows) = library.feeds.borrow_mut().remove(&id) {
                    for window in windows {
                        if let Some(window) = window.upgrade() {
                            window.close();
                        }
                    }
                }
                library.refresh();
            },
        )
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
    let css = gtk::CssProvider::new();
    css.load_from_data(
        ".grid-video-label, .grid-status {
            color: white;
            text-shadow: -1px -1px 2px black, 1px 1px 2px black, 0 0 3px black;
        }
        .grid-controls button {
            color: white;
            -gtk-icon-shadow: -1px -1px 2px black, 1px 1px 2px black, 0 0 3px black;
        }
        .grid-controls button.recording {
            color: @error_color;
        }
        .grid-controls scale trough {
            border: 1px solid alpha(black, 0.75);
            box-shadow: 0 1px 3px alpha(black, 0.8);
        }
        .grid-controls scale slider {
            box-shadow: 0 1px 3px alpha(black, 0.8);
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
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
    grid.set_homogeneous(true);
    grid.set_max_children_per_line(4);
    grid.set_column_spacing(GRID_SPACING as u32);
    grid.set_row_spacing(GRID_SPACING as u32);
    grid.set_margin_top(GRID_MARGIN);
    grid.set_margin_bottom(GRID_MARGIN);
    grid.set_margin_start(GRID_MARGIN);
    grid.set_margin_end(GRID_MARGIN);
    grid.set_hexpand(true);
    grid.set_valign(gtk::Align::Start);
    let grid_wrapper = gtk::Box::new(gtk::Orientation::Vertical, 0);
    grid_wrapper.set_halign(gtk::Align::Center);
    grid_wrapper.set_valign(gtk::Align::Start);
    grid_wrapper.append(&grid);
    let scroll = gtk::ScrolledWindow::builder().child(&grid_wrapper).build();
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
        grid_wrapper,
        scroll: scroll.clone(),
        layout_pending: Cell::new(false),
        layout_size: Cell::new((0, 0)),
        toasts,
        header_add: add.downgrade(),
        store: start_store_worker(),
        sources: RefCell::new(Vec::new()),
        busy: Cell::new(false),
        ready: Cell::new(false),
        feeds: RefCell::new(HashMap::new()),
        tiles: RefCell::new(HashMap::new()),
        shutting_down: Cell::new(false),
        next_id: Cell::new(Some(1)),
        loading: Cell::new(false),
        load_status,
        load_retry: load_retry.clone(),
        empty_add: empty_add.clone(),
    });
    for adjustment in [scroll.hadjustment(), scroll.vadjustment()] {
        let weak = Rc::downgrade(&library);
        adjustment.connect_changed(move |_| {
            if let Some(library) = weak.upgrade() {
                library.queue_grid_layout();
            }
        });
    }
    let weak = Rc::downgrade(&library);
    window.connect_close_request(move |_| {
        if let Some(library) = weak.upgrade() {
            library.shutting_down.set(true);
            for tile in library.tiles.borrow().values() {
                tile.resume.set(false);
                tile.playback.close();
            }
        }
        glib::Propagation::Proceed
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

#[cfg(test)]
mod grid_layout_tests {
    use super::grid_layout;

    #[test]
    fn uses_height_to_choose_larger_tiles() {
        let ratio = 16.0 / 9.0;
        assert_eq!(grid_layout(750, 1000, 2, ratio).0, 1);
        assert_eq!(grid_layout(1500, 600, 2, ratio).0, 2);
        assert_eq!(grid_layout(900, 500, 2, ratio).0, 2);
        assert_eq!(grid_layout(900, 1100, 2, ratio).0, 1);
    }

    #[test]
    fn reserves_height_for_portrait_feeds() {
        let landscape = grid_layout(1100, 1000, 2, 16.0 / 9.0);
        let portrait = grid_layout(1100, 1000, 2, 9.0 / 16.0);
        assert_eq!(landscape.0, 1);
        assert_eq!(portrait.0, 2);
    }

    #[test]
    fn keeps_tiles_usable_when_scrolling_is_necessary() {
        let (columns, width) = grid_layout(750, 200, 8, 16.0 / 9.0);
        assert_eq!(columns, 2);
        assert_eq!(width, 702);
    }
}
