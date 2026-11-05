use crate::managers::history::{HistoryEntry, HistoryManager};
use crate::managers::tts::TTSManager;
use crate::server_settings;
use crate::settings;
use crate::tray_i18n::get_tray_translations;
use log::{error, info, warn};
use std::sync::Arc;
use tauri::image::Image;
use tauri::menu::{
    CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Manager, Theme};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[derive(Clone, Debug, PartialEq)]
pub enum AppTheme {
    Dark,
    Light,
    Colored, // Pink/colored theme for Linux
}

/// Gets the current app theme, with Linux defaulting to Colored theme
pub fn get_current_theme(app: &AppHandle) -> AppTheme {
    if cfg!(target_os = "linux") {
        // On Linux, always use the colored theme
        AppTheme::Colored
    } else {
        // On other platforms, map system theme to our app theme
        if let Some(main_window) = app.get_webview_window("main") {
            match main_window.theme().unwrap_or(Theme::Dark) {
                Theme::Light => AppTheme::Light,
                Theme::Dark => AppTheme::Dark,
                _ => AppTheme::Dark, // Default fallback
            }
        } else {
            AppTheme::Dark
        }
    }
}

/// Gets the appropriate icon path for the given theme and state
pub fn get_icon_path(theme: AppTheme) -> &'static str {
    match theme {
        AppTheme::Dark => "resources/tray_idle.png",
        AppTheme::Light => "resources/tray_idle_dark.png",
        AppTheme::Colored => "resources/parrot.png",
    }
}

pub fn change_tray_icon(app: &AppHandle) {
    let tray = app.state::<TrayIcon>();
    let theme = get_current_theme(app);

    let icon_path = get_icon_path(theme);

    let _ = tray.set_icon(Some(
        Image::from_path(
            app.path()
                .resolve(icon_path, tauri::path::BaseDirectory::Resource)
                .expect("failed to resolve"),
        )
        .expect("failed to set icon"),
    ));

    update_tray_menu(app, None);
}

pub fn update_tray_menu(app: &AppHandle, locale: Option<&str>) {
    let settings = settings::get_settings(app);

    let locale = locale.unwrap_or(&settings.app_language);
    let strings = get_tray_translations(Some(locale.to_string()));

    // Platform-specific accelerators
    #[cfg(target_os = "macos")]
    let (settings_accelerator, quit_accelerator) = (Some("Cmd+,"), Some("Cmd+Q"));
    #[cfg(not(target_os = "macos"))]
    let (settings_accelerator, quit_accelerator) = (Some("Ctrl+,"), Some("Ctrl+Q"));

    // Create common menu items
    let version_label = if cfg!(debug_assertions) {
        format!("Parrot v{} (Dev)", env!("CARGO_PKG_VERSION"))
    } else {
        format!("Parrot v{}", env!("CARGO_PKG_VERSION"))
    };
    let version_i = MenuItem::with_id(app, "version", &version_label, false, None::<&str>)
        .expect("failed to create version item");
    let settings_i = MenuItem::with_id(
        app,
        "settings",
        &strings.settings,
        true,
        settings_accelerator,
    )
    .expect("failed to create settings item");
    let check_updates_i = MenuItem::with_id(
        app,
        "check_updates",
        &strings.check_updates,
        settings.update_checks_enabled,
        None::<&str>,
    )
    .expect("failed to create check updates item");
    let copy_last_text_i = MenuItem::with_id(
        app,
        "copy_last_text",
        &strings.copy_last_text,
        true,
        None::<&str>,
    )
    .expect("failed to create copy last transcript item");
    let model_loaded = app.state::<Arc<TTSManager>>().is_model_loaded();
    let unload_model_i = MenuItem::with_id(
        app,
        "unload_model",
        &strings.unload_model,
        model_loaded,
        None::<&str>,
    )
    .expect("failed to create unload model item");
    let quit_i = MenuItem::with_id(app, "quit", &strings.quit, true, quit_accelerator)
        .expect("failed to create quit item");
    let separator = || PredefinedMenuItem::separator(app).expect("failed to create separator");

    // --- Speech server (ttsd) section ---------------------------------
    let server_menu = build_server_menu(app, &strings);

    let sep1 = separator();
    let sep2 = separator();
    let sep3 = separator();
    let sep_server = separator();

    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![
        &version_i,
        &sep1,
        &copy_last_text_i,
        &unload_model_i,
        &sep2,
    ];
    if let Some(menu) = &server_menu {
        items.push(&sep_server);
        items.push(menu);
    }
    items.push(&sep3);
    items.push(&settings_i);
    items.push(&check_updates_i);
    items.push(&quit_i);

    let menu = Menu::with_items(app, &items).expect("failed to create menu");

    let tray = app.state::<TrayIcon>();
    let _ = tray.set_menu(Some(menu));
    let _ = tray.set_icon_as_template(true);
}

fn last_transcript_text(entry: &HistoryEntry) -> &str {
    &entry.transcription_text
}

pub fn set_tray_visibility(app: &AppHandle, visible: bool) {
    let tray = app.state::<TrayIcon>();
    if let Err(e) = tray.set_visible(visible) {
        error!("Failed to set tray visibility: {}", e);
    } else {
        info!("Tray visibility set to: {}", visible);
    }
}

pub fn copy_last_text(app: &AppHandle) {
    let history_manager = app.state::<Arc<HistoryManager>>();
    let entry = match history_manager.get_latest_entry() {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            warn!("No history entries available for tray copy.");
            return;
        }
        Err(err) => {
            error!("Failed to fetch last history entry: {}", err);
            return;
        }
    };

    if let Err(err) = app.clipboard().write_text(last_transcript_text(&entry)) {
        error!("Failed to copy last transcript to clipboard: {}", err);
        return;
    }

    info!("Copied last transcript to clipboard via tray.");
}

/// Builds the nested "Speech Server" submenu from the daemon snapshot:
/// model list (checkmark on the active one), voices of the active model,
/// language list and temperature steps. Greyed-out entries mark missing
/// clone references and an offline daemon.
fn build_server_menu(
    app: &AppHandle,
    strings: &crate::tray_i18n::TrayStrings,
) -> Option<Submenu<tauri::Wry>> {
    let (status, config) = server_settings::tray_server_snapshot();
    let header = if status.alive {
        strings.server.clone()
    } else {
        format!("{} ({})", strings.server, strings.server_offline)
    };

    let mut items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();

    let Some(config) = config else {
        // Daemon offline: one disabled info entry instead of an empty menu.
        let offline = MenuItem::with_id(
            app,
            "server:offline",
            &format!("{} ({})", strings.server, strings.server_offline),
            false,
            None::<&str>,
        )
        .ok()?;
        items.push(Box::new(offline));
        return Submenu::with_id_and_items(app, "server", &header, true, &ref_items(&items)).ok();
    };

    let active = config.active_model.clone().unwrap_or_default();

    // --- Model submenu ---
    let mut model_items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();
    for model in &config.models {
        let id = format!("server:model:{}", model.id);
        let checked = model.id == active;
        let mut label = model.alias.clone();
        // flag models whose clone references are all missing
        if model.engine_kind == "chatterbox" && model.clones.iter().all(|c| c.missing) {
            label = format!("{} ({})", model.alias, strings.server_missing_sample);
        }
        if let Ok(item) = CheckMenuItem::with_id(app, &id, &label, true, checked, None::<&str>) {
            model_items.push(Box::new(item));
        }
    }
    if !model_items.is_empty() {
        if let Ok(sub) =
            Submenu::with_id_and_items(app, "server-models", &strings.server_models, true, &ref_items(&model_items))
        {
            items.push(Box::new(sub));
        }
    }

    // --- Voice submenu for the active model ---
    if let Some(model) = config.models.iter().find(|m| m.id == active) {
        let mut voice_items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();
        let current_voice = model.default_voice.clone().unwrap_or_default();
        let mut voices = model.voices.clone();
        for clone in &model.clones {
            if !voices.contains(&clone.name) {
                voices.push(clone.name.clone());
            }
        }
        // User voice samples work with every model that clones.
        for sample in config.samples.iter().filter(|s| !s.missing) {
            let name = format!("{}{}", crate::model_families::server::CLONE_VOICE_PREFIX, sample.name);
            if !voices.contains(&name) {
                voices.push(name);
            }
        }
        for voice in voices {
            let missing = model
                .clones
                .iter()
                .any(|c| c.name == voice && c.missing);
            let label = if missing {
                format!("{} ({})", voice, strings.server_missing_sample)
            } else {
                voice.clone()
            };
            let id = format!("server:voice:{}:{}", model.id, voice);
            let checked = voice == current_voice;
            if let Ok(item) =
                CheckMenuItem::with_id(app, &id, &label, !missing, checked, None::<&str>)
            {
                voice_items.push(Box::new(item));
            }
        }
        if !voice_items.is_empty() {
            if let Ok(sub) = Submenu::with_id_and_items(
                app,
                "server-voices",
                &strings.server_voices,
                true,
                &ref_items(&voice_items),
            ) {
                items.push(Box::new(sub));
            }
        }
    }

    // --- Language submenu ---
    if let Some(model) = config.models.iter().find(|m| m.id == active) {
        let current_lang = config.lang_code.clone().unwrap_or_default();
        let mut lang_items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();
        for lang in &model.languages {
            let id = format!("server:lang:{}", lang);
            let checked = *lang == current_lang;
            if let Ok(item) =
                CheckMenuItem::with_id(app, &id, lang, true, checked, None::<&str>)
            {
                lang_items.push(Box::new(item));
            }
        }
        if !lang_items.is_empty() {
            if let Ok(sub) = Submenu::with_id_and_items(
                app,
                "server-langs",
                &strings.server_language,
                true,
                &ref_items(&lang_items),
            ) {
                items.push(Box::new(sub));
            }
        }

        // --- Temperature submenu ---
        let current_temp = config.temperature.unwrap_or(0.2);
        let mut temp_items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();
        for step in [0.1, 0.2, 0.3, 0.5, 0.7] {
            let id = format!("server:temp:{step}");
            let label = format!("{step}");
            let checked = (current_temp - step).abs() < 0.05;
            if let Ok(item) = CheckMenuItem::with_id(app, &id, &label, true, checked, None::<&str>)
            {
                temp_items.push(Box::new(item));
            }
        }
        if let Ok(sub) = Submenu::with_id_and_items(
            app,
            "server-temp",
            &strings.server_temperature,
            true,
            &ref_items(&temp_items),
        ) {
            items.push(Box::new(sub));
        }
    }

    Submenu::with_id_and_items(app, "server", &header, true, &ref_items(&items)).ok()
}

/// Turns boxed menu items into references Menu::with_items accepts.
fn ref_items(
    items: &[Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>],
) -> Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> {
    items.iter().map(|i| i.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::last_transcript_text;
    use crate::managers::history::HistoryEntry;

    fn build_entry(transcription: &str) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            file_name: "parrot-1.wav".to_string(),
            timestamp: 0,
            saved: false,
            title: "Recording".to_string(),
            transcription_text: transcription.to_string(),
        }
    }

    #[test]
    fn returns_transcript_text() {
        let entry = build_entry("raw");
        assert_eq!(last_transcript_text(&entry), "raw");
    }
}
