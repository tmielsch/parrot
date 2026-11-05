//! Speech-server (ttsd) integration for the tray and settings UI.
//!
//! The daemon `ttsd` (see ~/ttsd) exposes a control API on
//! `server.control_port` (default 8124) with `GET /config`,
//! `GET /status`, `POST /config` (JSON merge) and `POST /reload`.
//! This module mirrors that API as Tauri commands so the tray menu
//! and the settings window can both drive the daemon: switch the
//! active model, pick a voice, change language or temperature, and
//! swap clone references — all without restarting the daemon.

use log::{info, warn};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tauri::Manager;

use crate::managers::tts::TTSManager;
use crate::model_families::server::MODEL_ID_PREFIX;
use crate::settings::{get_settings, write_settings};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct ServerStatus {
    pub alive: bool,
    pub host: String,
    pub port: u16,
    pub models_loaded: Vec<String>,
}

/// A minimal view of the daemon config — the parts the UI needs.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type, Default)]
pub struct ServerConfigView {
    pub active_model: Option<String>,
    pub lang_code: Option<String>,
    pub temperature: Option<f32>,
    /// Fixed random seed for every request; None = fresh noise each time.
    pub seed: Option<i64>,
    /// Built-in voices use their anchor clip (steady across sentences).
    pub lock_voice: bool,
    pub models: Vec<ServerModelView>,
    /// User voice samples, selectable as voice "clone:<name>".
    pub samples: Vec<ServerSampleView>,
}

/// One user voice sample (global "clones" section of the daemon config).
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type, Default)]
pub struct ServerSampleView {
    pub name: String,
    pub ref_audio: String,
    pub ref_text: String,
    /// Original file the sample was imported from.
    pub source: Option<String>,
    /// True when the converted audio file is gone.
    pub missing: bool,
    /// Set when the import cut a long file: (original, kept) seconds.
    pub trimmed_from: Option<f32>,
    pub trimmed_to: Option<f32>,
}

/// Generation knobs of one model; None = the model's own default.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type, Default)]
pub struct ServerParamsView {
    pub top_p: Option<f32>,
    pub top_k: Option<i64>,
    pub repetition_penalty: Option<f32>,
    pub cfg_scale: Option<f32>,
    pub ddpm_steps: Option<i64>,
    pub sigma: Option<f32>,
    pub exaggeration: Option<f32>,
    pub cfg_weight: Option<f32>,
    pub min_p: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type, Default)]
pub struct ServerModelView {
    pub id: String,
    pub repo: String,
    pub alias: String,
    pub engine_kind: String,
    pub voices: Vec<String>,
    pub default_voice: Option<String>,
    pub languages: Vec<String>,
    pub temperature: f32,
    /// Clone reference names with a human label for the menu.
    pub clones: Vec<ServerCloneView>,
    pub params: ServerParamsView,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type, Default)]
pub struct ServerCloneView {
    pub name: String,
    /// True when the reference file is missing (menu shows it greyed).
    pub missing: bool,
}

fn control_base_url() -> Result<String, String> {
    // The daemon config decides host/control_port; reuse the family's
    // reader so the app and the daemon agree on one source of truth.
    let cfg = crate::model_families::server::load_public_server_config();
    let host = cfg.host.unwrap_or_else(|| "127.0.0.1".into());
    let port = cfg.control_port.unwrap_or(8124);
    Ok(format!("http://{host}:{port}"))
}

fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(CONTROL_TIMEOUT)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

/// Reads the daemon status (GET /status). Returns an offline marker
/// instead of an error so the tray can show "offline" calmly.
#[tauri::command]
#[specta::specta]
pub fn get_server_status() -> Result<ServerStatus, String> {
    let base = control_base_url()?;
    let client = client()?;
    match client.get(format!("{base}/status")).send() {
        Ok(resp) => {
            let status: ServerStatus = resp
                .json()
                .map_err(|e| format!("bad status response: {e}"))?;
            Ok(status)
        }
        Err(_) => Ok(ServerStatus {
            alive: false,
            host: "127.0.0.1".into(),
            port: 8123,
            models_loaded: vec![],
        }),
    }
}

/// Reads the daemon config as a UI-friendly view (GET /config).
#[tauri::command]
#[specta::specta]
pub fn get_server_config() -> Result<ServerConfigView, String> {
    let base = control_base_url()?;
    let client = client()?;
    let raw: serde_json::Value = client
        .get(format!("{base}/config"))
        .send()
        .map_err(|e| format!("ttsd control API not reachable: {e}"))?
        .json()
        .map_err(|e| format!("bad config response: {e}"))?;
    Ok(parse_config_view(&raw))
}

fn parse_config_view(raw: &serde_json::Value) -> ServerConfigView {
    let defaults = raw.get("defaults").cloned().unwrap_or_default();
    let mut models = Vec::new();
    if let Some(list) = raw.get("models").and_then(|m| m.as_array()) {
        for entry in list {
            let mut clones = Vec::new();
            if let Some(map) = entry.get("clones").and_then(|c| c.as_object()) {
                for (name, def) in map {
                    let missing = def
                        .get("placeholder")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);
                    clones.push(ServerCloneView {
                        name: name.clone(),
                        missing,
                    });
                }
            }
            models.push(ServerModelView {
                id: str_field(entry, "id"),
                repo: str_field(entry, "repo"),
                alias: str_field(entry, "alias"),
                engine_kind: str_field(entry, "engine_kind"),
                voices: str_array(entry, "voices"),
                default_voice: entry
                    .get("default_voice")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                languages: str_array(entry, "languages"),
                temperature: entry
                    .get("temperature")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(0.2),
                clones,
                params: entry
                    .get("params")
                    .cloned()
                    .and_then(|p| serde_json::from_value(p).ok())
                    .unwrap_or_default(),
            });
        }
    }
    let mut samples = Vec::new();
    if let Some(map) = raw.get("clones").and_then(|c| c.as_object()) {
        for (name, def) in map {
            let ref_audio = str_field(def, "ref_audio");
            let missing = !std::path::Path::new(&expand_home(&ref_audio)).is_file();
            samples.push(ServerSampleView {
                name: name.clone(),
                ref_audio,
                ref_text: str_field(def, "ref_text"),
                source: def.get("source").and_then(|s| s.as_str()).map(String::from),
                missing,
                trimmed_from: trimmed_value(def, "original_seconds"),
                trimmed_to: trimmed_value(def, "kept_seconds"),
            });
        }
    }
    samples.sort_by(|a, b| a.name.cmp(&b.name));
    ServerConfigView {
        seed: defaults.get("seed").and_then(|v| v.as_i64()),
        lock_voice: defaults
            .get("lock_voice")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        samples,
        active_model: raw
            .get("active_model")
            .and_then(|v| v.as_str())
            .map(String::from),
        lang_code: defaults
            .get("lang_code")
            .and_then(|v| v.as_str())
            .map(String::from),
        temperature: defaults
            .get("temperature")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32),
        models,
    }
}

fn trimmed_value(def: &serde_json::Value, key: &str) -> Option<f32> {
    def.get("trimmed")?.get(key)?.as_f64().map(|v| v as f32)
}

fn expand_home(raw: &str) -> String {
    match raw.strip_prefix("~/") {
        Some(rest) => format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest),
        None => raw.to_string(),
    }
}

fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or_default().into()
}

fn str_array(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Applies a JSON patch to the daemon config (POST /config + reload).
fn patch_config(patch: serde_json::Value) -> Result<(), String> {
    let base = control_base_url()?;
    let client = client()?;
    client
        .post(format!("{base}/config"))
        .json(&patch)
        .send()
        .map_err(|e| format!("ttsd control API not reachable: {e}"))?
        .error_for_status()
        .map_err(|e| format!("config patch rejected: {e}"))?;
    client
        .post(format!("{base}/reload"))
        .send()
        .map_err(|e| format!("reload failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("reload rejected: {e}"))?;
    Ok(())
}

/// Switches the active server model (tray: "server:model:<id>").
///
/// Writes `active_model` to the daemon config and makes the app select
/// the matching `server--<id>` model, swapping the loaded engine the same
/// way the Models screen does (`commands::models::set_active_model`).
#[tauri::command]
#[specta::specta]
pub fn set_server_model(app: AppHandle, model_id: String) -> Result<(), String> {
    let model_id = model_id
        .trim_start_matches(MODEL_ID_PREFIX)
        .trim()
        .to_string();
    patch_config(serde_json::json!({ "active_model": model_id }))?;

    let full_id = format!("{MODEL_ID_PREFIX}{model_id}");
    let mut settings = get_settings(&app);
    settings.selected_model = full_id.clone();
    write_settings(&app, settings);

    let tts = app.state::<Arc<TTSManager>>();
    if tts.is_model_loaded() && tts.get_current_model().as_deref() != Some(full_id.as_str()) {
        tts.unload_model().map_err(|e| e.to_string())?;
    }
    tts.initiate_model_load();
    info!("Server model switched to {}", model_id);
    Ok(())
}

/// Sets the voice of one server model (tray: "server:voice:<name>").
///
/// Only that model's `default_voice` changes (POST /model keeps its other
/// fields, e.g. clone references). The server engine reads the voice from
/// the daemon config on every sentence, so no app setting is touched.
#[tauri::command]
#[specta::specta]
pub fn set_server_voice(model_id: String, voice: String) -> Result<(), String> {
    let model_id = model_id.trim_start_matches(MODEL_ID_PREFIX).trim().to_string();
    post_json("/model", serde_json::json!({ "id": model_id, "default_voice": voice }))?;
    post_json("/reload", serde_json::json!({}))
}

/// Sets the language code sent with every request (tray: "server:lang:<code>").
/// Stored only in the daemon config; Parrot's own `selected_language`
/// stays untouched so Kokoro/Piper keep their settings.
#[tauri::command]
#[specta::specta]
pub fn set_server_language(lang_code: String) -> Result<(), String> {
    patch_config(serde_json::json!({ "defaults": { "lang_code": lang_code } }))
}

fn post_json(path: &str, body: serde_json::Value) -> Result<(), String> {
    let base = control_base_url()?;
    client()?
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .map_err(|e| format!("ttsd control API not reachable: {e}"))?
        .error_for_status()
        .map_err(|e| format!("ttsd rejected {path}: {e}"))?;
    Ok(())
}

/// Sets the default temperature (tray: "temp:<value>").
#[tauri::command]
#[specta::specta]
pub fn set_server_temperature(temperature: f32) -> Result<(), String> {
    patch_config(serde_json::json!({ "defaults": { "temperature": temperature } }))
}

/// Sets the fixed seed (None = random noise per request).
#[tauri::command]
#[specta::specta]
pub fn set_server_seed(seed: Option<i64>) -> Result<(), String> {
    patch_config(serde_json::json!({ "defaults": { "seed": seed } }))
}

/// Turns the anchor clips of built-in voices on or off.
#[tauri::command]
#[specta::specta]
pub fn set_server_lock_voice(lock_voice: bool) -> Result<(), String> {
    patch_config(serde_json::json!({ "defaults": { "lock_voice": lock_voice } }))
}

/// Replaces the generation knobs of one model. Absent (None) knobs fall
/// back to the model's own defaults, so the whole object is written.
#[tauri::command]
#[specta::specta]
pub fn set_server_params(model_id: String, params: ServerParamsView) -> Result<(), String> {
    let model_id = model_id.trim_start_matches(MODEL_ID_PREFIX).trim().to_string();
    let knobs = serde_json::to_value(&params).map_err(|e| e.to_string())?;
    // POST /model deep-merges dicts, so a cleared knob (null) must be sent
    // explicitly; serde writes None as null here, which overwrites it.
    post_json("/model", serde_json::json!({ "id": model_id, "params": knobs }))?;
    post_json("/reload", serde_json::json!({}))
}

/// Imports an audio file as a user voice sample (POST /clone). The daemon
/// converts it to WAV with afconvert, so wav/mp3/m4a/aiff/flac all work.
#[tauri::command]
#[specta::specta]
pub fn add_server_sample(name: String, source_path: String, ref_text: String) -> Result<ServerSampleView, String> {
    let base = control_base_url()?;
    // Long files are cut with Whisper word timestamps on the daemon side;
    // the first Whisper call loads the model, so allow a few minutes.
    let language = crate::model_families::server::load_server_config_lang();
    let resp = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?
        .post(format!("{base}/clone"))
        .json(&serde_json::json!({ "name": name, "source_path": source_path, "ref_text": ref_text, "language": language }))
        .send()
        .map_err(|e| format!("ttsd control API not reachable: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().map_err(|e| format!("bad response: {e}"))?;
    if !status.is_success() {
        return Err(body.get("error").and_then(|e| e.as_str()).unwrap_or("import failed").to_string());
    }
    Ok(ServerSampleView {
        name: str_field(&body, "name"),
        ref_audio: str_field(&body, "ref_audio"),
        ref_text: str_field(&body, "ref_text"),
        source: body.get("source").and_then(|s| s.as_str()).map(String::from),
        missing: false,
        trimmed_from: trimmed_value(&body, "original_seconds"),
        trimmed_to: trimmed_value(&body, "kept_seconds"),
    })
}

/// Removes a user voice sample and its converted file.
#[tauri::command]
#[specta::specta]
pub fn remove_server_sample(name: String) -> Result<(), String> {
    post_json("/clone/delete", serde_json::json!({ "name": name }))
}

/// Whisper transcript of an audio file, suggested as the sample's text.
/// The first call loads Whisper on the daemon side (can take ~30 s).
#[tauri::command]
#[specta::specta]
pub fn transcribe_server_sample(path: String, language: Option<String>) -> Result<String, String> {
    let base = control_base_url()?;
    let resp = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?
        .post(format!("{base}/transcribe"))
        .json(&serde_json::json!({ "path": path, "language": language }))
        .send()
        .map_err(|e| format!("ttsd control API not reachable: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().map_err(|e| format!("bad response: {e}"))?;
    if !status.is_success() {
        return Err(body.get("error").and_then(|e| e.as_str()).unwrap_or("transcription failed").to_string());
    }
    Ok(str_field(&body, "text"))
}

/// Speaks a test sentence with the current server settings, exactly the
/// way the hotkey would (same engine, same overlay). Switches the app to
/// the active server model first when another engine is selected.
#[tauri::command]
#[specta::specta]
pub fn preview_server_voice(app: AppHandle, text: String) -> Result<(), String> {
    let tts = app.state::<Arc<TTSManager>>();
    let selected = get_settings(&app).selected_model;
    if !selected.starts_with(MODEL_ID_PREFIX) {
        return Err("select a Speech Server model first".into());
    }
    if !tts.is_model_loaded() && !tts.is_model_loading() {
        tts.initiate_model_load();
    }
    let Some(request_id) = tts.begin_request_or_toggle_stop() else {
        // A request was running; the call above stopped it.
        return Ok(());
    };
    // speak() runs synthesis and playback on its own thread.
    tts.speak(text, request_id);
    Ok(())
}

/// Forces a daemon reload (Settings: "Apply"/tray).
#[tauri::command]
#[specta::specta]
pub fn reload_server() -> Result<(), String> {
    let base = control_base_url()?;
    let client = client()?;
    client
        .post(format!("{base}/reload"))
        .send()
        .map_err(|e| format!("reload failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("reload rejected: {e}"))?;
    Ok(())
}

/// Convenience for the tray: status + config in one call, tolerating
/// an offline daemon (returns None config in that case).
pub fn tray_server_snapshot() -> (ServerStatus, Option<ServerConfigView>) {
    let status = get_server_status().unwrap_or(ServerStatus {
        alive: false,
        host: "127.0.0.1".into(),
        port: 8123,
        models_loaded: vec![],
    });
    let config = if status.alive {
        get_server_config().ok()
    } else {
        warn!("ttsd daemon is offline");
        None
    };
    (status, config)
}

use tauri::AppHandle;
