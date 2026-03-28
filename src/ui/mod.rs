mod preferences;
mod state;
mod timers;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::Config;
use crate::metadata;

use state::*;

pub fn build_ui(app: &adw::Application) {
    let state = Rc::new(RefCell::new(AppState::default()));

    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
    let css_provider = gtk4::CssProvider::new();
    css_provider.load_from_data(include_str!("../style.css"));
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().expect("Could not get default display"),
        &css_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("RUMP")
        .default_width(420)
        .icon_name("goodvibes")
        .build();

    let toolbar_view = adw::ToolbarView::new();

    // Header
    let header = adw::HeaderBar::new();
    let title_widget = adw::WindowTitle::new("RUMP", "Icecast Streaming Client");
    header.set_title_widget(Some(&title_widget));
    let menu = gtk4::gio::Menu::new();
    menu.append(Some("Toggle Console"), Some("app.toggle-console"));
    menu.append(Some("Preferences"), Some("app.preferences"));
    menu.append(Some("About RUMP"), Some("app.about"));
    header.pack_end(
        &gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .build(),
    );
    toolbar_view.add_top_bar(&header);

    // Content
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(16)
        .margin_start(16)
        .margin_end(16)
        .margin_top(8)
        .margin_bottom(16)
        .build();
    toolbar_view.set_content(Some(&content));

    // Control row
    let start_button = gtk4::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .css_classes(["circular", "suggested-action"])
        .tooltip_text("Start Streaming")
        .valign(gtk4::Align::Center)
        .build();

    let mic_indicator = gtk4::Button::builder()
        .icon_name("audio-input-microphone-symbolic")
        .css_classes(["circular", "flat"])
        .tooltip_text("Mic (click or hold PTT key)")
        .valign(gtk4::Align::Center)
        .build();

    let title_label = gtk4::Label::builder()
        .label("Not Playing")
        .css_classes(["heading"])
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();

    let artist_label = gtk4::Label::builder()
        .label("")
        .css_classes(["dim-label", "caption"])
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();

    let now_playing = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(1)
        .hexpand(true)
        .valign(gtk4::Align::Center)
        .build();
    now_playing.append(&title_label);
    now_playing.append(&artist_label);

    let timer_label = gtk4::Label::builder()
        .label("00:00:00")
        .css_classes(["timer-label", "heading"])
        .valign(gtk4::Align::Center)
        .build();

    let control_row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(12)
        .valign(gtk4::Align::Center)
        .build();
    control_row.append(&start_button);
    control_row.append(&mic_indicator);
    control_row.append(&now_playing);
    control_row.append(&timer_label);
    content.append(&control_row);

    // Mic toggle (click to toggle, independent of PTT hold)
    {
        let state = state.clone();
        mic_indicator.connect_clicked(move |_| {
            let s = state.borrow();
            let current = s.is_mic_toggled.load(Ordering::Relaxed);
            s.is_mic_toggled.store(!current, Ordering::Relaxed);
        });
    }

    // VU Meters
    let build_vu_row = |label_text: &str| -> (gtk4::Box, gtk4::LevelBar) {
        let meter = gtk4::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .value(0.0)
            .hexpand(true)
            .build();
        meter.add_offset_value("level-yellow", 0.7);
        meter.add_offset_value("level-red", 0.9);
        let row = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .build();
        row.append(
            &gtk4::Label::builder()
                .label(label_text)
                .css_classes(["vu-label"])
                .build(),
        );
        row.append(&meter);
        (row, meter)
    };

    let vu_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    let (left_row, left_meter) = build_vu_row("L");
    let (right_row, right_meter) = build_vu_row("R");
    let (mic_row, mic_meter) = build_vu_row("M");
    vu_box.append(&left_row);
    vu_box.append(&right_row);
    vu_box.append(&mic_row);
    content.append(&vu_box);

    // Console
    let log_buffer = gtk4::TextBuffer::new(None);
    let log_view = gtk4::TextView::builder()
        .buffer(&log_buffer)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .css_classes(["dim-label"])
        .wrap_mode(gtk4::WrapMode::WordChar)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    let log_scroll = gtk4::ScrolledWindow::builder()
        .child(&log_view)
        .min_content_height(120)
        .max_content_height(300)
        .vexpand(true)
        .build();
    content.append(&log_scroll);

    // ── Actions ──────────────────────────────────────────────────
    {
        let ls = log_scroll.clone();
        let a = gtk4::gio::SimpleAction::new("toggle-console", None);
        a.connect_activate(move |_, _| ls.set_visible(!ls.is_visible()));
        app.add_action(&a);
    }
    {
        let w = window.clone();
        let a = gtk4::gio::SimpleAction::new("preferences", None);
        a.connect_activate(move |_, _| preferences::open_preferences(&w));
        app.add_action(&a);
    }
    {
        let s = state.clone();
        let a = gtk4::gio::SimpleAction::new("restart-capture", None);
        a.connect_activate(move |_, _| {
            let config = Config::load();
            let mut s = s.borrow_mut();
            if config.input_device.is_empty() || s.is_streaming || config.input_device == s.current_device {
                return;
            }
            let devices = crate::pipewire::list_devices().unwrap_or_default();
            if let Some(dev) = devices.iter().find(|d| d.display_name == config.input_device) {
                start_capture(&mut s, dev.serial, &config);
            }
        });
        app.add_action(&a);
    }
    {
        let s = state.clone();
        let a = gtk4::gio::SimpleAction::new("restart-mic", None);
        a.connect_activate(move |_, _| {
            let config = Config::load();
            let mut s = s.borrow_mut();
            if s.is_streaming {
                return;
            }
            if config.mic_device != s.current_mic_device {
                if config.mic_device.is_empty() {
                    stop_mic_capture(&mut s);
                } else {
                    let sources = crate::pipewire::list_sources().unwrap_or_default();
                    if let Some(dev) = sources.iter().find(|d| d.display_name == config.mic_device) {
                        start_mic_capture(&mut s, dev.serial, &config);
                    }
                }
            }
            start_ptt(&mut s, &config);
        });
        app.add_action(&a);
    }
    {
        let w = window.clone();
        let a = gtk4::gio::SimpleAction::new("about", None);
        a.connect_activate(move |_, _| {
            let about = adw::AboutWindow::builder()
                .transient_for(&w)
                .modal(true)
                .application_name("RUMP")
                .application_icon("goodvibes")
                .version(env!("CARGO_PKG_VERSION"))
                .developer_name("Shaun Lastra")
                .license_type(gtk4::License::MitX11)
                .comments("Icecast streaming client with DJ mic mixing")
                .build();
            about.present();
        });
        app.add_action(&a);
    }

    // ── Startup ──────────────────────────────────────────────────
    {
        let mut s = state.borrow_mut();
        let config = Config::load();

        if !config.input_device.is_empty() {
            let devices = crate::pipewire::list_devices().unwrap_or_default();
            if let Some(dev) = devices.iter().find(|d| d.display_name == config.input_device) {
                start_capture(&mut s, dev.serial, &config);
            }
        }
        if !config.mic_device.is_empty() {
            let sources = crate::pipewire::list_sources().unwrap_or_default();
            if let Some(dev) = sources.iter().find(|d| d.display_name == config.mic_device) {
                start_mic_capture(&mut s, dev.serial, &config);
            }
        }
        start_ptt(&mut s, &config);
        s.metadata_thread = Some(metadata::spawn_metadata_listener(
            s.metadata.clone(),
            s.metadata_stop.clone(),
            s.log.clone(),
        ));
    }

    // ── Graceful Shutdown ────────────────────────────────────────
    {
        let state = state.clone();
        window.connect_destroy(move |_| {
            shutdown(&mut state.borrow_mut());
        });
    }

    // ── Start/Stop Handler ───────────────────────────────────────
    {
        let state = state.clone();
        let start_button = start_button.clone();
        let timer_label = timer_label.clone();
        let title_widget = title_widget.clone();

        start_button.connect_clicked(move |btn| {
            let mut s = state.borrow_mut();

            if s.is_streaming {
                stop_streaming(&mut s);
                reset_to_idle(btn, &timer_label, &title_widget, "Icecast Streaming Client");
            } else {
                match start_streaming(&mut s) {
                    Ok(()) => {
                        btn.set_icon_name("media-playback-stop-symbolic");
                        btn.set_css_classes(&["circular", "destructive-action"]);
                        btn.set_tooltip_text(Some("Stop Streaming"));
                        title_widget.set_subtitle("Live");
                    }
                    Err(msg) => title_widget.set_subtitle(&msg),
                }
            }
        });
    }

    // ── Timers ───────────────────────────────────────────────────
    timers::setup_timers(
        state.clone(),
        left_meter,
        right_meter,
        mic_meter,
        mic_indicator,
        title_label,
        artist_label,
        start_button,
        timer_label,
        log_buffer,
        log_view,
        title_widget,
    );

    window.set_content(Some(&toolbar_view));
    window.present();
}
