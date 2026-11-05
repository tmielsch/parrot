use log::{debug, warn};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use specta::Type;
use std::collections::HashMap;
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

// Custom deserializer to handle both old numeric format (1-5) and new string format ("trace", "debug", etc.)
impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LogLevelVisitor;

        impl<'de> Visitor<'de> for LogLevelVisitor {
            type Value = LogLevel;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string or integer representing log level")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<LogLevel, E> {
                match value.to_lowercase().as_str() {
                    "trace" => Ok(LogLevel::Trace),
                    "debug" => Ok(LogLevel::Debug),
                    "info" => Ok(LogLevel::Info),
                    "warn" => Ok(LogLevel::Warn),
                    "error" => Ok(LogLevel::Error),
                    _ => Err(E::unknown_variant(
                        value,
                        &["trace", "debug", "info", "warn", "error"],
                    )),
                }
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<LogLevel, E> {
                match value {
                    1 => Ok(LogLevel::Trace),
                    2 => Ok(LogLevel::Debug),
                    3 => Ok(LogLevel::Info),
                    4 => Ok(LogLevel::Warn),
                    5 => Ok(LogLevel::Error),
                    _ => Err(E::invalid_value(de::Unexpected::Unsigned(value), &"1-5")),
                }
            }
        }

        deserializer.deserialize_any(LogLevelVisitor)
    }
}

impl From<LogLevel> for tauri_plugin_log::LogLevel {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Trace => tauri_plugin_log::LogLevel::Trace,
            LogLevel::Debug => tauri_plugin_log::LogLevel::Debug,
            LogLevel::Info => tauri_plugin_log::LogLevel::Info,
            LogLevel::Warn => tauri_plugin_log::LogLevel::Warn,
            LogLevel::Error => tauri_plugin_log::LogLevel::Error,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct ShortcutBinding {
    pub id: String,
    pub name: String,
    pub description: String,
    pub default_binding: String,
    pub current_binding: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "lowercase")]
pub enum OverlayPosition {
    None,
    Top,
    Bottom,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum ModelUnloadTimeout {
    #[default]
    Never,
    Immediately,
    // serde's snake_case turns `Min2` into "min2" while specta exports "min_2",
    // so spell the wire names out to keep Rust and the TS bindings in agreement.
    #[serde(rename = "min_2", alias = "min2")]
    Min2,
    #[serde(rename = "min_5", alias = "min5")]
    Min5,
    #[serde(rename = "min_10", alias = "min10")]
    Min10,
    #[serde(rename = "min_15", alias = "min15")]
    Min15,
    #[serde(rename = "hour_1", alias = "hour1")]
    Hour1,
    #[serde(rename = "sec_5", alias = "sec5")]
    Sec5, // Debug mode only
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRetentionPeriod {
    Never,
    PreserveLimit,
    Days3,
    Weeks2,
    Months3,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardImplementation {
    Tauri,
    HandyKeys,
}

impl Default for KeyboardImplementation {
    fn default() -> Self {
        // Default to HandyKeys on macOS and Windows. Linux uses Tauri by default
        // (HandyKeys is disabled on Linux due to Wayland instability).
        #[cfg(target_os = "linux")]
        return KeyboardImplementation::Tauri;
        #[cfg(not(target_os = "linux"))]
        return KeyboardImplementation::HandyKeys;
    }
}

impl ModelUnloadTimeout {
    /// Parse a UI or stored timeout value.
    ///
    /// Accepts both serde snake_case (`min_5`) and the compact UI leftovers
    /// (`min5`) so a settings write round-trips after restart.
    pub fn from_setting_value(value: &str) -> Result<Self, String> {
        serde_json::from_value(serde_json::Value::String(value.to_string()))
            .map_err(|_| format!("Invalid model unload timeout: {value}"))
    }

    pub fn to_minutes(self) -> Option<u64> {
        match self {
            ModelUnloadTimeout::Never => None,
            ModelUnloadTimeout::Immediately => Some(0), // Special case for immediate unloading
            ModelUnloadTimeout::Min2 => Some(2),
            ModelUnloadTimeout::Min5 => Some(5),
            ModelUnloadTimeout::Min10 => Some(10),
            ModelUnloadTimeout::Min15 => Some(15),
            ModelUnloadTimeout::Hour1 => Some(60),
            ModelUnloadTimeout::Sec5 => Some(0), // Special case for debug - handled separately
        }
    }

    pub fn to_seconds(self) -> Option<u64> {
        match self {
            ModelUnloadTimeout::Never => None,
            ModelUnloadTimeout::Immediately => Some(0), // Special case for immediate unloading
            ModelUnloadTimeout::Sec5 => Some(5),
            _ => self.to_minutes().map(|m| m * 60),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum SoundTheme {
    Marimba,
    Pop,
    Custom,
}

impl SoundTheme {
    fn as_str(&self) -> &'static str {
        match self {
            SoundTheme::Marimba => "marimba",
            SoundTheme::Pop => "pop",
            SoundTheme::Custom => "custom",
        }
    }

    pub fn start_path(self) -> String {
        format!("resources/{}_start.wav", self.as_str())
    }

    pub fn stop_path(self) -> String {
        format!("resources/{}_stop.wav", self.as_str())
    }
}

/* still handy for composing the initial JSON in the store ------------- */
#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct AppSettings {
    pub bindings: HashMap<String, ShortcutBinding>,
    pub audio_feedback: bool,
    #[serde(default = "default_audio_feedback_volume")]
    pub audio_feedback_volume: f32,
    #[serde(default = "default_sound_theme")]
    pub sound_theme: SoundTheme,
    #[serde(default = "default_start_hidden")]
    pub start_hidden: bool,
    #[serde(default = "default_autostart_enabled")]
    pub autostart_enabled: bool,
    #[serde(default = "default_update_checks_enabled")]
    pub update_checks_enabled: bool,
    #[serde(default = "default_model")]
    pub selected_model: String,
    #[serde(default)]
    pub selected_output_device: Option<String>,
    #[serde(default = "default_selected_language")]
    pub selected_language: String,
    #[serde(default)]
    pub selected_kokoro_voice: Option<String>,
    #[serde(default = "default_show_close_button")]
    pub show_close_button: bool,
    #[serde(default = "default_overlay_position")]
    pub overlay_position: OverlayPosition,
    #[serde(default = "default_debug_mode")]
    pub debug_mode: bool,
    #[serde(default = "default_log_level")]
    pub log_level: LogLevel,
    #[serde(default)]
    pub model_unload_timeout: ModelUnloadTimeout,
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
    #[serde(
        default = "default_history_retention_period",
        alias = "recording_retention_period"
    )]
    pub history_retention_period: HistoryRetentionPeriod,
    #[serde(default = "default_app_language")]
    pub app_language: String,
    #[serde(default)]
    pub keyboard_implementation: KeyboardImplementation,
    #[serde(default = "default_show_tray_icon")]
    pub show_tray_icon: bool,
    #[serde(default = "default_tts_speed")]
    pub tts_speed: f32,
    #[serde(default = "default_tts_shorten_first_chunk")]
    pub tts_shorten_first_chunk: bool,
    /// When playback starts: right after the first sentence, once enough
    /// audio is buffered to play through without gaps, or after the
    /// whole text is rendered.
    #[serde(default)]
    pub tts_playback_mode: PlaybackMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, Type)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackMode {
    /// Start after the first sentence (Parrot's original behaviour).
    Stream,
    /// Start once the buffered audio covers the estimated remaining render
    /// time, so playback does not stall between sentences.
    #[default]
    Buffered,
    /// Render everything, then play it in one piece.
    Complete,
}

fn default_model() -> String {
    "".to_string()
}

fn default_start_hidden() -> bool {
    false
}

fn default_autostart_enabled() -> bool {
    false
}

fn default_update_checks_enabled() -> bool {
    true
}

fn default_selected_language() -> String {
    "auto".to_string()
}

fn default_overlay_position() -> OverlayPosition {
    #[cfg(target_os = "linux")]
    return OverlayPosition::None;
    #[cfg(not(target_os = "linux"))]
    return OverlayPosition::Bottom;
}

fn default_show_close_button() -> bool {
    true
}

fn default_debug_mode() -> bool {
    false
}

fn default_log_level() -> LogLevel {
    LogLevel::Debug
}

fn default_history_limit() -> usize {
    3
}

fn default_history_retention_period() -> HistoryRetentionPeriod {
    HistoryRetentionPeriod::PreserveLimit
}

fn default_audio_feedback_volume() -> f32 {
    1.0
}

fn default_sound_theme() -> SoundTheme {
    SoundTheme::Marimba
}

fn default_app_language() -> String {
    tauri_plugin_os::locale()
        .map(|l| l.replace('_', "-"))
        .unwrap_or_else(|| "en".to_string())
}

fn default_show_tray_icon() -> bool {
    true
}

fn default_tts_speed() -> f32 {
    1.0
}

fn default_tts_shorten_first_chunk() -> bool {
    true
}

pub const SETTINGS_STORE_PATH: &str = "settings_store.json";

pub fn get_default_settings() -> AppSettings {
    #[cfg(target_os = "windows")]
    let default_shortcut = "ctrl+space";
    #[cfg(target_os = "macos")]
    let default_shortcut = "option+space";
    #[cfg(target_os = "linux")]
    let default_shortcut = "ctrl+space";
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let default_shortcut = "alt+space";

    let mut bindings = HashMap::new();
    bindings.insert(
        "speak".to_string(),
        ShortcutBinding {
            id: "speak".to_string(),
            name: "Read Aloud Shortcut".to_string(),
            description: "Read the selected text aloud. Press again to stop.".to_string(),
            default_binding: default_shortcut.to_string(),
            current_binding: default_shortcut.to_string(),
        },
    );
    #[cfg(target_os = "macos")]
    let default_play_pause_binding = "option+p";
    #[cfg(not(target_os = "macos"))]
    let default_play_pause_binding = "alt+p";

    bindings.insert(
        "play_pause".to_string(),
        ShortcutBinding {
            id: "play_pause".to_string(),
            name: "Play/Pause Shortcut".to_string(),
            description: "Pause or resume speech playback.".to_string(),
            default_binding: default_play_pause_binding.to_string(),
            current_binding: "disabled".to_string(),
        },
    );

    AppSettings {
        bindings,
        audio_feedback: false,
        audio_feedback_volume: default_audio_feedback_volume(),
        sound_theme: default_sound_theme(),
        start_hidden: default_start_hidden(),
        autostart_enabled: default_autostart_enabled(),
        update_checks_enabled: default_update_checks_enabled(),
        selected_model: "".to_string(),
        selected_output_device: None,
        selected_language: "auto".to_string(),
        selected_kokoro_voice: None,
        overlay_position: default_overlay_position(),
        debug_mode: false,
        log_level: default_log_level(),
        model_unload_timeout: ModelUnloadTimeout::Never,
        history_limit: default_history_limit(),
        history_retention_period: default_history_retention_period(),
        app_language: default_app_language(),
        keyboard_implementation: KeyboardImplementation::default(),
        show_tray_icon: default_show_tray_icon(),
        tts_speed: default_tts_speed(),
        tts_shorten_first_chunk: default_tts_shorten_first_chunk(),
        tts_playback_mode: PlaybackMode::default(),
        show_close_button: default_show_close_button(),
    }
}

pub fn load_or_create_app_settings(app: &AppHandle) -> AppSettings {
    // Initialize store
    let store = app
        .store(SETTINGS_STORE_PATH)
        .expect("Failed to initialize store");

    let settings = if let Some(settings_value) = store.get("settings") {
        // Parse the entire settings object
        match serde_json::from_value::<AppSettings>(settings_value) {
            Ok(mut settings) => {
                debug!("Found existing settings: {:?}", settings);
                let default_settings = get_default_settings();
                let mut updated = false;

                // Merge default bindings into existing settings
                for (key, value) in default_settings.bindings {
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        settings.bindings.entry(key)
                    {
                        debug!("Adding missing binding: {}", e.key());
                        e.insert(value);
                        updated = true;
                    }
                }

                if updated {
                    debug!("Settings updated with new bindings");
                    store.set("settings", serde_json::to_value(&settings).unwrap());
                }

                settings
            }
            Err(e) => {
                warn!("Failed to parse settings: {}", e);
                // Fall back to default settings if parsing fails
                let default_settings = get_default_settings();
                store.set("settings", serde_json::to_value(&default_settings).unwrap());
                default_settings
            }
        }
    } else {
        let default_settings = get_default_settings();
        store.set("settings", serde_json::to_value(&default_settings).unwrap());
        default_settings
    };

    settings
}

pub fn get_settings(app: &AppHandle) -> AppSettings {
    let store = app
        .store(SETTINGS_STORE_PATH)
        .expect("Failed to initialize store");

    let mut settings = if let Some(settings_value) = store.get("settings") {
        serde_json::from_value::<AppSettings>(settings_value).unwrap_or_else(|_| {
            let default_settings = get_default_settings();
            store.set("settings", serde_json::to_value(&default_settings).unwrap());
            default_settings
        })
    } else {
        let default_settings = get_default_settings();
        store.set("settings", serde_json::to_value(&default_settings).unwrap());
        default_settings
    };

    let mut dirty = false;

    // Migrate old "transcribe" binding ID to "speak"
    if settings.bindings.contains_key("transcribe") {
        if let Some(mut binding) = settings.bindings.remove("transcribe") {
            binding.id = "speak".to_string();
            settings.bindings.insert("speak".to_string(), binding);
        }
        dirty = true;
    }

    if dirty {
        store.set("settings", serde_json::to_value(&settings).unwrap());
    }

    settings
}

pub fn write_settings(app: &AppHandle, settings: AppSettings) {
    let store = app
        .store(SETTINGS_STORE_PATH)
        .expect("Failed to initialize store");

    store.set("settings", serde_json::to_value(&settings).unwrap());
}

pub fn get_bindings(app: &AppHandle) -> HashMap<String, ShortcutBinding> {
    let settings = get_settings(app);

    settings.bindings
}

pub fn get_stored_binding(app: &AppHandle, id: &str) -> ShortcutBinding {
    let bindings = get_bindings(app);

    let binding = bindings.get(id).unwrap().clone();

    binding
}

pub fn get_history_limit(app: &AppHandle) -> usize {
    let settings = get_settings(app);
    settings.history_limit
}

pub fn get_history_retention_period(app: &AppHandle) -> HistoryRetentionPeriod {
    let settings = get_settings(app);
    settings.history_retention_period
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TIMEOUTS: [ModelUnloadTimeout; 8] = [
        ModelUnloadTimeout::Never,
        ModelUnloadTimeout::Immediately,
        ModelUnloadTimeout::Min2,
        ModelUnloadTimeout::Min5,
        ModelUnloadTimeout::Min10,
        ModelUnloadTimeout::Min15,
        ModelUnloadTimeout::Hour1,
        ModelUnloadTimeout::Sec5,
    ];

    #[test]
    fn model_unload_timeout_round_trips_through_json() {
        for timeout in ALL_TIMEOUTS {
            let wire = serde_json::to_value(timeout).unwrap();
            let parsed: ModelUnloadTimeout = serde_json::from_value(wire).unwrap();
            assert_eq!(parsed, timeout);
        }
    }

    #[test]
    fn model_unload_timeout_durations() {
        assert_eq!(ModelUnloadTimeout::Never.to_seconds(), None);
        assert_eq!(ModelUnloadTimeout::Immediately.to_seconds(), Some(0));
        assert_eq!(ModelUnloadTimeout::Sec5.to_seconds(), Some(5));
        assert_eq!(ModelUnloadTimeout::Min2.to_seconds(), Some(120));
        assert_eq!(ModelUnloadTimeout::Min15.to_seconds(), Some(900));
        assert_eq!(ModelUnloadTimeout::Hour1.to_seconds(), Some(3600));
    }

    #[test]
    fn default_settings_round_trip_through_json() {
        let defaults = get_default_settings();
        let json = serde_json::to_value(&defaults).unwrap();
        let parsed: AppSettings = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
        assert_eq!(parsed.model_unload_timeout, ModelUnloadTimeout::Never);
        assert!(parsed.bindings.contains_key("speak"));
    }

    #[test]
    fn every_model_unload_timeout_survives_settings_round_trip() {
        for timeout in ALL_TIMEOUTS {
            let mut settings = get_default_settings();
            settings.model_unload_timeout = timeout;
            let json = serde_json::to_value(&settings).unwrap();
            let parsed: AppSettings = serde_json::from_value(json).unwrap();
            assert_eq!(parsed.model_unload_timeout, timeout);
        }
    }

    #[test]
    fn settings_missing_newer_fields_fall_back_to_defaults() {
        let defaults = get_default_settings();
        let minimal = serde_json::json!({
            "bindings": serde_json::to_value(&defaults.bindings).unwrap(),
            "audio_feedback": false,
        });
        let parsed: AppSettings = serde_json::from_value(minimal).unwrap();
        assert_eq!(parsed.model_unload_timeout, ModelUnloadTimeout::Never);
        assert_eq!(parsed.selected_model, defaults.selected_model);
        assert_eq!(parsed.tts_speed, defaults.tts_speed);
        assert_eq!(
            parsed.tts_shorten_first_chunk,
            defaults.tts_shorten_first_chunk
        );
        assert_eq!(parsed.history_limit, defaults.history_limit);
    }

    #[test]
    fn model_unload_timeout_wire_names_match_typescript_bindings() {
        let cases = [
            (ModelUnloadTimeout::Never, "never"),
            (ModelUnloadTimeout::Immediately, "immediately"),
            (ModelUnloadTimeout::Min2, "min_2"),
            (ModelUnloadTimeout::Min5, "min_5"),
            (ModelUnloadTimeout::Min10, "min_10"),
            (ModelUnloadTimeout::Min15, "min_15"),
            (ModelUnloadTimeout::Hour1, "hour_1"),
            (ModelUnloadTimeout::Sec5, "sec_5"),
        ];
        let bindings = include_str!("../../src/bindings.ts");
        for (timeout, wire) in cases {
            assert_eq!(serde_json::to_value(timeout).unwrap(), wire);
            let parsed: ModelUnloadTimeout =
                serde_json::from_value(serde_json::json!(wire)).unwrap();
            assert_eq!(parsed, timeout);
            assert!(
                bindings.contains(&format!("\"{wire}\"")),
                "{wire} missing from bindings.ts"
            );
        }
    }

    #[test]
    fn model_unload_timeout_accepts_legacy_names() {
        for (legacy, expected) in [
            ("min2", ModelUnloadTimeout::Min2),
            ("min5", ModelUnloadTimeout::Min5),
            ("min10", ModelUnloadTimeout::Min10),
            ("min15", ModelUnloadTimeout::Min15),
            ("hour1", ModelUnloadTimeout::Hour1),
            ("sec5", ModelUnloadTimeout::Sec5),
        ] {
            let parsed: ModelUnloadTimeout =
                serde_json::from_value(serde_json::json!(legacy)).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn model_unload_timeout_survives_settings_round_trip() {
        let mut settings = get_default_settings();
        settings.model_unload_timeout = ModelUnloadTimeout::Min5;
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["model_unload_timeout"], "min_5");
        let parsed: AppSettings = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.model_unload_timeout, ModelUnloadTimeout::Min5);
    }

    #[test]
    fn parses_unload_timeout_setting_from_either_form() {
        assert_eq!(
            ModelUnloadTimeout::from_setting_value("min_5").unwrap(),
            ModelUnloadTimeout::Min5
        );
        assert_eq!(
            ModelUnloadTimeout::from_setting_value("min5").unwrap(),
            ModelUnloadTimeout::Min5
        );
        assert!(ModelUnloadTimeout::from_setting_value("min3").is_err());
    }
}
