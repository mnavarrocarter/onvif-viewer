use adw::prelude::*;
use adw::{Application, ApplicationWindow};

const APP_ID: &str = "io.github.onvif_viewer.OnvifViewer";

fn main() -> adw::glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let header = adw::HeaderBar::new();
    let layout = adw::ToolbarView::new();
    layout.add_top_bar(&header);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("ONVIF Viewer")
        .default_width(900)
        .default_height(600)
        .content(&layout)
        .build();
    window.present();
}
