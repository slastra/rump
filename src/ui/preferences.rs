use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::{Codec, Config};
use crate::pipewire::PwDevice;
use crate::ptt;

const OPUS_BITRATE_OPTIONS: [u32; 5] = [64, 96, 128, 192, 256];

pub(crate) fn open_preferences(parent: &adw::ApplicationWindow) {
    let config = Config::load();

    let prefs_window = adw::Window::builder()
        .title("Preferences")
        .transient_for(parent)
        .modal(true)
        .default_width(600)
        .default_height(700)
        .build();

    let toolbar_view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let view_stack = adw::ViewStack::new();
    let view_switcher = adw::ViewSwitcher::builder().stack(&view_stack).build();
    header.set_title_widget(Some(&view_switcher));
    toolbar_view.add_top_bar(&header);

    // ── Audio Tab ────────────────────────────────────────────────
    let audio_page = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical).spacing(16)
        .margin_start(16).margin_end(16).margin_top(8).margin_bottom(16).build();

    let devices: Vec<PwDevice> = crate::pipewire::list_devices().unwrap_or_default();
    let device_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    let selected_device_idx = Rc::new(std::cell::Cell::new(0usize));
    let mut first_check: Option<gtk4::CheckButton> = None;
    let saved_idx = devices.iter().position(|d| d.display_name == config.input_device);
    for (i, device) in devices.iter().enumerate() {
        let check = gtk4::CheckButton::new();
        if let Some(ref first) = first_check { check.set_group(Some(first)); }
        else { first_check = Some(check.clone()); }
        if saved_idx == Some(i) { check.set_active(true); selected_device_idx.set(i); }
        let idx = selected_device_idx.clone();
        check.connect_toggled(move |c| { if c.is_active() { idx.set(i); } });
        let row = adw::ActionRow::builder().title(&device.display_name).activatable_widget(&check).build();
        row.add_prefix(&check);
        device_list.append(&row);
    }
    let device_group = adw::PreferencesGroup::builder().title("Input Device").build();
    device_group.add(&device_list);
    audio_page.append(&device_group);

    let encoding_group = adw::PreferencesGroup::builder().title("Encoding").build();

    let codec_list = gtk4::StringList::new(&["Vorbis", "Opus"]);
    let codec_row = adw::ComboRow::builder().title("Codec").model(&codec_list).build();
    codec_row.set_selected(match config.codec { Codec::Opus => 1, Codec::Vorbis => 0 });

    let sr_list = gtk4::StringList::new(&["44100", "48000"]);
    let sr_row = adw::ComboRow::builder().title("Sample Rate").model(&sr_list).build();
    sr_row.set_selected(if config.sample_rate == 48000 { 1 } else { 0 });
    let ch_list = gtk4::StringList::new(&["Mono", "Stereo"]);
    let ch_row = adw::ComboRow::builder().title("Channels").model(&ch_list).build();
    ch_row.set_selected(if config.channels == 1 { 0 } else { 1 });

    let q_scale = gtk4::Scale::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .adjustment(&gtk4::Adjustment::new(config.vorbis_quality as f64, 0.0, 1.0, 0.1, 0.1, 0.0))
        .draw_value(true).digits(1).hexpand(true).valign(gtk4::Align::Center).build();
    let q_row = adw::ActionRow::builder().title("Vorbis Quality").build();
    q_row.add_suffix(&q_scale);

    let br_labels: Vec<String> = OPUS_BITRATE_OPTIONS.iter().map(u32::to_string).collect();
    let br_label_refs: Vec<&str> = br_labels.iter().map(String::as_str).collect();
    let br_list = gtk4::StringList::new(&br_label_refs);
    let br_row = adw::ComboRow::builder().title("Opus Bitrate (kbps)").model(&br_list).build();
    br_row.set_selected(
        OPUS_BITRATE_OPTIONS.iter()
            .position(|&b| b == config.opus_bitrate_kbps)
            .unwrap_or(2) as u32,
    );

    encoding_group.add(&codec_row);
    encoding_group.add(&sr_row);
    encoding_group.add(&ch_row);
    encoding_group.add(&q_row);
    encoding_group.add(&br_row);
    audio_page.append(&encoding_group);

    let audio_scroll = gtk4::ScrolledWindow::builder().child(&audio_page).vexpand(true).build();
    view_stack.add_titled(&audio_scroll, Some("audio"), "Audio");
    view_stack.page(&audio_scroll).set_icon_name(Some("audio-speakers-symbolic"));

    // ── Microphone Tab ───────────────────────────────────────────
    let mic_page = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical).spacing(16)
        .margin_start(16).margin_end(16).margin_top(8).margin_bottom(16).build();

    let sources: Vec<PwDevice> = crate::pipewire::list_sources().unwrap_or_default();
    let mic_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    let selected_mic_idx = Rc::new(std::cell::Cell::new(None::<usize>));
    let saved_mic_idx = sources.iter().position(|d| d.display_name == config.mic_device);

    let none_check = gtk4::CheckButton::new();
    let first_mic_check = none_check.clone();
    if saved_mic_idx.is_none() { none_check.set_active(true); }
    { let idx = selected_mic_idx.clone();
      none_check.connect_toggled(move |c| { if c.is_active() { idx.set(None); } }); }
    let none_row = adw::ActionRow::builder().title("None (disabled)").activatable_widget(&none_check).build();
    none_row.add_prefix(&none_check);
    mic_list.append(&none_row);

    for (i, source) in sources.iter().enumerate() {
        let check = gtk4::CheckButton::new();
        check.set_group(Some(&first_mic_check));
        if saved_mic_idx == Some(i) { check.set_active(true); selected_mic_idx.set(Some(i)); }
        let idx = selected_mic_idx.clone();
        check.connect_toggled(move |c| { if c.is_active() { idx.set(Some(i)); } });
        let row = adw::ActionRow::builder().title(&source.display_name).activatable_widget(&check).build();
        row.add_prefix(&check);
        mic_list.append(&row);
    }

    let mic_device_group = adw::PreferencesGroup::builder().title("Microphone").build();
    mic_device_group.add(&mic_list);
    mic_page.append(&mic_device_group);

    // PTT key combo
    let ptt_group = adw::PreferencesGroup::builder().title("Push-to-Talk").build();
    let combo_parts: Vec<&str> = config.ptt_key.split('+').collect();
    let (saved_modifier, saved_key) = if combo_parts.len() == 2 {
        (combo_parts[0], combo_parts[1])
    } else {
        ("None", combo_parts[0])
    };
    let mod_list = gtk4::StringList::new(ptt::PTT_MODIFIER_OPTIONS);
    let mod_row = adw::ComboRow::builder().title("Modifier").model(&mod_list).build();
    mod_row.set_selected(ptt::PTT_MODIFIER_OPTIONS.iter()
        .position(|m| m.eq_ignore_ascii_case(saved_modifier)).unwrap_or(0) as u32);
    let key_list = gtk4::StringList::new(ptt::PTT_KEY_OPTIONS);
    let key_row = adw::ComboRow::builder().title("Key").model(&key_list).build();
    key_row.set_selected(ptt::PTT_KEY_OPTIONS.iter()
        .position(|k| k.eq_ignore_ascii_case(saved_key)).unwrap_or(0) as u32);
    ptt_group.add(&mod_row);
    ptt_group.add(&key_row);
    mic_page.append(&ptt_group);

    // Ducking
    let duck_group = adw::PreferencesGroup::builder().title("Ducking").build();
    let threshold_scale = gtk4::Scale::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .adjustment(&gtk4::Adjustment::new(config.duck_threshold as f64, 0.0, 0.1, 0.005, 0.01, 0.0))
        .draw_value(true).digits(3).hexpand(true).valign(gtk4::Align::Center).build();
    let threshold_row = adw::ActionRow::builder().title("Threshold").subtitle("Mic level to trigger ducking").build();
    threshold_row.add_suffix(&threshold_scale);
    let duck_level_scale = gtk4::Scale::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .adjustment(&gtk4::Adjustment::new(config.duck_level as f64, 0.0, 1.0, 0.05, 0.1, 0.0))
        .draw_value(true).digits(2).hexpand(true).valign(gtk4::Align::Center).build();
    let duck_level_row = adw::ActionRow::builder().title("Duck Level").subtitle("Music volume while ducked").build();
    duck_level_row.add_suffix(&duck_level_scale);
    let attack_list = gtk4::StringList::new(&["50", "100", "200"]);
    let attack_row = adw::ComboRow::builder().title("Attack (ms)").subtitle("How fast music ducks").model(&attack_list).build();
    attack_row.set_selected(match config.duck_attack_ms { 50 => 0, 200 => 2, _ => 1 });
    let release_list = gtk4::StringList::new(&["400", "800", "1500"]);
    let release_row = adw::ComboRow::builder().title("Release (ms)").subtitle("How fast music returns").model(&release_list).build();
    release_row.set_selected(match config.duck_release_ms { 400 => 0, 1500 => 2, _ => 1 });
    let hold_list = gtk4::StringList::new(&["300", "500", "1000"]);
    let hold_row = adw::ComboRow::builder().title("Hold (ms)").subtitle("Stay ducked after mic goes silent").model(&hold_list).build();
    hold_row.set_selected(match config.duck_hold_ms { 300 => 0, 1000 => 2, _ => 1 });
    duck_group.add(&threshold_row);
    duck_group.add(&duck_level_row);
    duck_group.add(&attack_row);
    duck_group.add(&release_row);
    duck_group.add(&hold_row);
    mic_page.append(&duck_group);

    let mic_scroll = gtk4::ScrolledWindow::builder().child(&mic_page).vexpand(true).build();
    view_stack.add_titled(&mic_scroll, Some("mic"), "Microphone");
    view_stack.page(&mic_scroll).set_icon_name(Some("audio-input-microphone-symbolic"));

    // ── Server Tab ───────────────────────────────────────────────
    let server_page = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical).spacing(16)
        .margin_start(16).margin_end(16).margin_top(8).margin_bottom(16).build();
    let server_group = adw::PreferencesGroup::builder().title("Icecast Server").build();
    let host_row = adw::EntryRow::builder().title("Host").text(&config.host).build();
    let port_row = adw::EntryRow::builder().title("Port").text(&config.port.to_string()).build();
    let mount_row = adw::EntryRow::builder().title("Mount").text(&config.mount).build();
    let pass_row = adw::PasswordEntryRow::builder().title("Password").text(&config.password).build();
    server_group.add(&host_row);
    server_group.add(&port_row);
    server_group.add(&mount_row);
    server_group.add(&pass_row);
    server_page.append(&server_group);
    let server_scroll = gtk4::ScrolledWindow::builder().child(&server_page).vexpand(true).build();
    view_stack.add_titled(&server_scroll, Some("server"), "Server");
    view_stack.page(&server_scroll).set_icon_name(Some("network-server-symbolic"));

    toolbar_view.set_content(Some(&view_stack));
    let done_button = gtk4::Button::builder()
        .label("Done").css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Center).margin_top(8).margin_bottom(8).build();
    { let w = prefs_window.clone(); done_button.connect_clicked(move |_| w.close()); }
    toolbar_view.add_bottom_bar(&done_button);
    prefs_window.set_content(Some(&toolbar_view));

    // Save on close
    prefs_window.connect_close_request(move |w| {
        let input_device = devices.get(selected_device_idx.get()).map(|d| d.display_name.clone()).unwrap_or_default();
        let mic_device = selected_mic_idx.get().and_then(|i| sources.get(i)).map(|d| d.display_name.clone()).unwrap_or_default();
        let ptt_modifier = ptt::PTT_MODIFIER_OPTIONS.get(mod_row.selected() as usize).unwrap_or(&"None");
        let ptt_trigger = ptt::PTT_KEY_OPTIONS.get(key_row.selected() as usize).unwrap_or(&"Space");
        let ptt_key = if *ptt_modifier == "None" { ptt_trigger.to_string() } else { format!("{ptt_modifier}+{ptt_trigger}") };
        let attack_ms: u32 = ["50", "100", "200"][attack_row.selected() as usize].parse().unwrap_or(100);
        let release_ms: u32 = ["400", "800", "1500"][release_row.selected() as usize].parse().unwrap_or(800);
        let hold_ms: u32 = ["300", "500", "1000"][hold_row.selected() as usize].parse().unwrap_or(500);

        Config {
            host: host_row.text().to_string(),
            port: port_row.text().to_string().parse().unwrap_or(8000),
            mount: mount_row.text().to_string(),
            password: pass_row.text().to_string(),
            input_device,
            codec: match codec_row.selected() { 1 => Codec::Opus, _ => Codec::Vorbis },
            sample_rate: match sr_row.selected() { 1 => 48000, _ => 44100 },
            channels: match ch_row.selected() { 0 => 1, _ => 2 },
            vorbis_quality: q_scale.value() as f32,
            opus_bitrate_kbps: OPUS_BITRATE_OPTIONS
                .get(br_row.selected() as usize).copied().unwrap_or(128),
            mic_device, ptt_key,
            duck_threshold: threshold_scale.value() as f32,
            duck_level: duck_level_scale.value() as f32,
            duck_attack_ms: attack_ms, duck_release_ms: release_ms, duck_hold_ms: hold_ms,
        }.save();

        if let Some(app) = w.transient_for().and_then(|w| w.application()) {
            if let Some(a) = app.lookup_action("restart-capture") { a.activate(None); }
            if let Some(a) = app.lookup_action("restart-mic") { a.activate(None); }
        }
        gtk4::glib::Propagation::Proceed
    });

    prefs_window.present();
}
