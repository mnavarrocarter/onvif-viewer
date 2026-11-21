# ONVIF Viewer

A native Linux desktop application for viewing ONVIF cameras with GTK4, libadwaita, and GStreamer.

## Screenshots

![Empty camera library](screenshots/empty-state.png)

![Camera library with two cameras](screenshots/cameras.png)

![Camera feed with private content obscured](screenshots/live-feed.png)

## Requirements

Rust stable, Cargo, `pkg-config`, GTK4, libadwaita 1.4 or later, and GStreamer development libraries are required to build. Playback also requires the GStreamer GTK4 paintable sink, RTSP, audio output and appropriate depayloader and decoder plugins for the camera's streams (for example H.264 and G.711 plugins).

## Run

```sh
cargo run
```

Select **Add Camera**, then enter its ONVIF device service URL including its port, username, and password. Select a camera tile to open its feed. Use the tile's settings button to edit or remove the camera, and its profile menu to choose a stream; changing the profile also switches any open feeds.

Camera tiles and newly opened feed windows use the selected profile's aspect ratio when its dimensions are known. On Wayland, the compositor controls interactive window resizing, so the window itself cannot be locked to that ratio; the video remains undistorted and is letterboxed when necessary.

The camera library, selected profiles, and credentials are kept in memory only and are lost when the application exits. Removing a camera closes its feed windows; closing a feed stops playback. A storage interface separates the library from its current in-memory implementation; no persistent backend is provided.
