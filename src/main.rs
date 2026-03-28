mod audio;
mod config;
mod log;
mod metadata;
mod pipewire;
mod ptt;
mod stream;
mod ui;

use gtk4::prelude::*;
use libadwaita as adw;

fn main() {
    let app = adw::Application::builder()
        .application_id("us.lastra.rump")
        .build();

    app.connect_activate(ui::build_ui);
    app.run();
}
