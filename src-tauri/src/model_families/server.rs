//! Remote TTS engine backed by an OpenAI-compatible speech server.
//!
//! The app keeps everything that makes Parrot Parrot (hotkey, selection
//! capture, overlay, playback, history) and adds a third engine whose
//! models live in a local daemon (`ttsd`) instead of inside the app
//! bundle. The engine talks to two endpoints:
//!
//! - `POST {base}/v1/audio/speech` — OpenAI-style synthesis. The request
//!   carries `model`, `input`, `voice`, `lang_code`, `temperature`,
//!   `speed` and `response_format: "wav"`; the response is a WAV body we
//!   decode with `hound`.
//! - `GET {base}/v1/models` — availability check during `load()` (kept
//!   short; the slow first synthesis happens afterwards in the warmup
//!   call, outside the model-load timeout path).
//!
//! Model discovery does not go through Hugging Face repos. Instead the
//! family reads the daemon's config file (`~/ttsd/server.yaml`) and
//! `list_server_models()` turns each entry into a `ModelInfo` whose
//! `filename` points at a directory inside the app's models dir (created
//! empty so `update_download_status` keeps it selectable — see the map
//! note in `managers/model.rs`). Voice lists, default voices, clone
//! references and language defaults all come from the same config, which
//! the tray can rewrite at runtime through the daemon's control API.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use hound::{SampleFormat, WavReader};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use tts_rs::SynthesisResult;

use super::{HfRepo, LoadContext, ModelFamily, SpeechEngine};
use crate::managers::model::{EngineType, ModelInfo};

/// Where the daemon keeps its configuration. Overridable for tests.
const CONFIG_PATH: &str = "ttsd/server.yaml";

/// Prefix of app-side model ids and model directories for server models
/// (`server--<config id>`).
pub const MODEL_ID_PREFIX: &str = "server--";

/// Timeout for the availability probe in `load()`. Must stay well below
/// the 30 s model-load deadline in `managers/tts.rs`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Timeout for synthesis requests. Cold model loads on the server take
/// ~40 s (measured), warm calls a few seconds, so this is generous.
const SYNTH_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------- config ---

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    server: ServerSection,
    #[serde(default)]
    defaults: DefaultsSection,
    #[serde(default)]
    models: Vec<ModelEntry>,
    /// User voice samples, usable by every model that can clone (Ming
    /// and Chatterbox). Selected as voice "clone:<name>".
    #[serde(default)]
    clones: HashMap<String, CloneRef>,
}

/// Voice names that mean "use the user voice sample <name>".
pub const CLONE_VOICE_PREFIX: &str = "clone:";

#[derive(Debug, Clone, Deserialize)]
struct ServerSection {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_control_port")]
    control_port: u16,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            control_port: default_control_port(),
        }
    }
}

impl ServerSection {
    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DefaultsSection {
    #[serde(default = "default_lang")]
    lang_code: String,
    /// Global temperature set from the tray; when absent, each model's
    /// own `temperature` applies.
    #[serde(default)]
    temperature: Option<f32>,
    /// Random seed sent with every request. One fixed seed for all
    /// sentences keeps the voice from drifting between them (measured:
    /// timbre distance over five sentences 16.7 free vs 13.3 seeded,
    /// with a voice sample). None = server picks fresh noise each time.
    #[serde(default)]
    seed: Option<i64>,
    /// Use the anchor clip of a built-in voice instead of its bare name.
    /// Steadier across sentences, but the anchor fixes one rendering of
    /// the voice, which may sound worse than the bare voice.
    #[serde(default = "default_true")]
    lock_voice: bool,
}

impl Default for DefaultsSection {
    fn default() -> Self {
        Self {
            lang_code: default_lang(),
            temperature: None,
            seed: None,
            lock_voice: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Generation knobs forwarded to the backend (`~/ttsd/backend.py`). Every
/// field is optional; an absent field leaves the model's own default.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GenerationParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetition_penalty: Option<f32>,
    /// Ming: classifier-free guidance (model default 2.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cfg_scale: Option<f32>,
    /// Ming: flow-matching steps (model default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ddpm_steps: Option<i64>,
    /// Ming: sampling noise width (model default 0.25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sigma: Option<f32>,
    /// Chatterbox: emotion exaggeration (model default 0.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exaggeration: Option<f32>,
    /// Chatterbox: guidance weight (model default 0.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cfg_weight: Option<f32>,
    /// Chatterbox: min-p sampling (model default 0.05).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub repo: String,
    pub alias: String,
    /// "ming" selects built-in voices; "chatterbox" requires a clone ref.
    pub engine_kind: String,
    #[serde(default)]
    pub voices: Vec<String>,
    #[serde(default)]
    pub default_voice: Option<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub clones: HashMap<String, CloneRef>,
    /// Anchor clips for built-in voices (`ttsd anchors <model>`): one
    /// fixed-seed rendering per voice, sent as ref_audio with every
    /// sentence so the speaker stays the same across a whole text.
    #[serde(default)]
    pub anchors: HashMap<String, CloneRef>,
    /// Per-model generation knobs, edited in the settings window.
    #[serde(default)]
    pub params: GenerationParams,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloneRef {
    pub ref_audio: String,
    #[serde(default)]
    pub ref_text: String,
    #[serde(default)]
    pub placeholder: bool,
}

fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    8123
}

fn default_control_port() -> u16 {
    8124
}

fn default_lang() -> String {
    "de".into()
}

fn default_temperature() -> f32 {
    0.2
}

/// Loads the daemon config from `~/{CONFIG_PATH}`. Missing file or
/// missing `models:` section yields an empty config (no server models).
pub fn load_server_config() -> ServerConfig {
    let path = PathBuf::from(home_dir()).join(CONFIG_PATH);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return ServerConfig::default();
    };
    serde_yaml::from_str(&raw).unwrap_or_default()
}

/// Minimal connection facts other modules need (host / speech port /
/// control port) without seeing the full config types.
pub struct PublicServerConfig {
    pub host: Option<String>,
    /// Control API port (`server.control_port`, default 8124).
    pub control_port: Option<u16>,
}

/// The voice the daemon config names for an app-side server model id
/// (`server--<id>`): `default_voice`, else the first usable clone.
pub fn configured_voice_for(app_model_id: &str) -> Option<String> {
    let id = app_model_id.strip_prefix(MODEL_ID_PREFIX)?;
    let cfg = load_server_config();
    let entry = cfg.models.iter().find(|m| m.id == id)?;
    entry
        .default_voice
        .clone()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            entry
                .clones
                .iter()
                .find(|(_, c)| !c.placeholder)
                .map(|(name, _)| name.clone())
        })
        .or_else(|| entry.voices.first().cloned())
}

/// Connection facts for the tray/settings layer.
pub fn load_public_server_config() -> PublicServerConfig {
    let cfg = load_server_config();
    PublicServerConfig {
        host: Some(cfg.server.host),
        control_port: Some(cfg.server.control_port),
    }
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".into())
}

/// Resolves `~`-prefixed paths in the config to absolute paths.
fn expand_ref_path(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("~/") {
        format!("{}/{}", home_dir(), rest)
    } else {
        raw.to_string()
    }
}

// ------------------------------------------------------------ model list ---

/// ModelInfos for every configured daemon model. `filename` is the
/// directory the app's ModelManager watches (created empty by the
/// manager when registering — see `register_server_models` callers).
pub fn list_server_models() -> Vec<ModelInfo> {
    let cfg = load_server_config();
    cfg.models.iter().map(entry_to_model_info).collect()
}

fn entry_to_model_info(entry: &ModelEntry) -> ModelInfo {
    ModelInfo {
        id: format!("{MODEL_ID_PREFIX}{}", entry.id),
        name: entry.alias.clone(),
        description: format!("Server model {} ({})", entry.repo, entry.engine_kind),
        filename: format!("{MODEL_ID_PREFIX}{}", entry.id),
        url: None,
        size_mb: 0,
        is_downloaded: true,
        is_downloading: false,
        partial_size: 0,
        is_directory: true,
        engine_type: EngineType::Server,
        accuracy_score: 0.9,
        speed_score: 0.7,
        is_recommended: false,
        supported_languages: entry.languages.clone(),
        is_custom: true,
        components: vec![],
    }
}

// ----------------------------------------------------------------- family ---

pub struct Server;

impl ModelFamily for Server {
    fn engine_type(&self) -> EngineType {
        EngineType::Server
    }

    fn layout(&self) -> &'static str {
        "a running ttsd daemon (see ~/ttsd/server.yaml)"
    }

    fn detect(&self, _repo: &HfRepo) -> Vec<ModelInfo> {
        // Server models come from the daemon config, not from HF repos.
        Vec::new()
    }

    fn load(&self, ctx: &LoadContext) -> Result<Box<dyn SpeechEngine>, String> {
        let cfg = load_server_config();
        let base_url = std::env::var("PARROT_SERVER_URL")
            .unwrap_or_else(|_| cfg.server.base_url());

        // The model dir is models/server--<id>; its name tells us which
        // configured server model the app selected.
        let selected_id = ctx
            .model_dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(MODEL_ID_PREFIX))
            .map(str::to_string);
        let selected_repo = selected_id
            .as_deref()
            .and_then(|id| cfg.models.iter().find(|m| m.id == id))
            .or_else(|| cfg.models.first())
            .map(|m| m.repo.clone());
        if selected_repo.is_none() {
            return Err("no server model configured in ~/ttsd/server.yaml".into());
        }

        // Quick availability probe; the heavy first synthesis happens in
        // the warmup call after load() returns (outside the 30 s load
        // deadline — see managers/tts.rs wait_for_pending_model_load).
        let client = Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        client
            .get(format!("{base_url}/v1/models"))
            .send()
            .map_err(|e| format!("ttsd not reachable at {base_url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("ttsd probe failed: {e}"))?;

        Ok(Box::new(ServerEngine {
            client: Client::builder()
                .timeout(SYNTH_TIMEOUT)
                .build()
                .map_err(|e| format!("failed to build HTTP client: {e}"))?,
            base_url,
            config: cfg,
            selected_repo,
        }))
    }
}

// ----------------------------------------------------------------- engine ---

pub struct ServerEngine {
    client: Client,
    base_url: String,
    config: ServerConfig,
    /// Repo id of the active model, set on the first synthesize call
    /// from the voice/style arguments (Parrot knows the model id only
    /// through its ModelManager, not through the engine trait).
    selected_repo: Option<String>,
}

impl ServerEngine {
    /// Re-reads the daemon config so tray changes (language, temperature,
    /// voice, clone files) take effect on the next sentence without
    /// reloading the engine. Falls back to the config seen at load time.
    fn refresh_config(&mut self) {
        let fresh = load_server_config();
        if !fresh.models.is_empty() {
            self.config = fresh;
        }
    }

    fn active_entry(&self) -> Option<&ModelEntry> {
        let repo = self.selected_repo.as_deref();
        self.config
            .models
            .iter()
            .find(|m| Some(m.repo.as_str()) == repo)
            .or_else(|| self.config.models.first())
    }

    fn synthesize_request(
        &self,
        entry: &ModelEntry,
        text: &str,
        voice: &str,
        speed: f32,
    ) -> Result<Vec<u8>, String> {
        let lang_code = std::env::var("PARROT_SERVER_LANG")
            .ok()
            .or_else(|| nonempty(&self.config.defaults.lang_code))
            .or_else(|| entry.languages.first().cloned())
            .unwrap_or_else(|| "de".into());
        // Global default wins so the tray's temperature menu applies to
        // every model; the per-model value is only a fallback.
        let temperature = self.config.defaults.temperature.unwrap_or(entry.temperature);

        let mut payload = serde_json::json!({
            "model": entry.repo,
            "input": text,
            "lang_code": lang_code,
            "temperature": temperature,
            "speed": speed,
            "response_format": "wav",
            "stream": false,
        });
        if let Some(seed) = self.config.defaults.seed {
            payload["seed"] = serde_json::json!(seed);
        }
        if let Ok(serde_json::Value::Object(knobs)) = serde_json::to_value(&entry.params) {
            for (key, value) in knobs {
                payload[key] = value;
            }
        }

        match self.resolve_reference(entry, voice)? {
            Some(reference) => {
                payload["voice"] = serde_json::Value::Null;
                payload["ref_audio"] = serde_json::json!(expand_ref_path(&reference.ref_audio));
                payload["ref_text"] = serde_json::json!(reference.ref_text);
            }
            None => payload["voice"] = serde_json::json!(voice),
        }

        let response = self
            .client
            .post(format!("{}/v1/audio/speech", self.base_url))
            .json(&payload)
            .send()
            .map_err(|e| format!("speech request failed: {e}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .map_err(|e| format!("failed to read speech response: {e}"))?;
        if !status.is_success() {
            let snippet = String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned();
            return Err(format!("ttsd returned {status}: {snippet}"));
        }
        Ok(body.to_vec())
    }
}

impl ServerEngine {
    /// Decides which reference clip (if any) goes with a request:
    ///
    /// 1. `clone:<name>` -> the user voice sample `clones[name]`.
    /// 2. Chatterbox -> the model's own clone `voice`, else the first
    ///    usable user sample (Chatterbox cannot speak without one).
    /// 3. A built-in voice with `lock_voice` on and an anchor clip on
    ///    disk -> the anchor (steady speaker across sentences).
    /// 4. Otherwise none: the bare voice name is sent.
    fn resolve_reference(&self, entry: &ModelEntry, voice: &str) -> Result<Option<CloneRef>, String> {
        if let Some(name) = voice.strip_prefix(CLONE_VOICE_PREFIX) {
            let clone = self
                .config
                .clones
                .get(name)
                .ok_or_else(|| format!("voice sample '{name}' is not configured"))?;
            return usable_clone(name, clone).map(Some);
        }

        if entry.engine_kind == "chatterbox" {
            if let Some(clone) = entry.clones.get(voice) {
                return usable_clone(voice, clone).map(Some);
            }
            let fallback = self
                .config
                .clones
                .iter()
                .find(|(name, clone)| usable_clone(name, clone).is_ok());
            return match fallback {
                Some((_, clone)) => Ok(Some(clone.clone())),
                None => Err(format!(
                    "{} needs a voice sample: add one in Settings > Speech Server",
                    entry.alias
                )),
            };
        }

        if self.config.defaults.lock_voice {
            if let Some(anchor) = entry.anchors.get(voice) {
                if usable_clone(voice, anchor).is_ok() {
                    return Ok(Some(anchor.clone()));
                }
            }
        }
        Ok(None)
    }
}

/// A clone reference is usable when it is not a placeholder and its
/// audio file exists. The error text is shown to the user.
fn usable_clone(name: &str, clone: &CloneRef) -> Result<CloneRef, String> {
    if clone.placeholder || clone.ref_audio.trim().is_empty() {
        return Err(format!("voice sample '{name}' has no audio file yet"));
    }
    let path = expand_ref_path(&clone.ref_audio);
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("voice sample '{name}' is missing: {path}"));
    }
    Ok(clone.clone())
}

fn nonempty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

impl SpeechEngine for ServerEngine {
    fn list_voices(&self) -> Vec<String> {
        let mut voices = match self.active_entry() {
            Some(entry) if !entry.voices.is_empty() => entry.voices.clone(),
            Some(entry) => entry.clones.keys().cloned().collect(),
            None => Vec::new(),
        };
        let mut samples: Vec<String> = self
            .config
            .clones
            .keys()
            .map(|name| format!("{CLONE_VOICE_PREFIX}{name}"))
            .collect();
        samples.sort();
        voices.extend(samples);
        voices
    }

    fn default_voice(&self) -> String {
        let Some(entry) = self.active_entry() else {
            return String::new();
        };
        entry
            .default_voice
            .clone()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                entry
                    .clones
                    .iter()
                    .find(|(_, c)| !c.placeholder)
                    .map(|(name, _)| name.clone())
            })
            .or_else(|| entry.voices.first().cloned())
            .unwrap_or_default()
    }

    fn synthesize(
        &mut self,
        text: &str,
        voice: &str,
        _style_index: usize,
        speed: f32,
    ) -> Result<SynthesisResult, String> {
        // Pick up tray changes (voice, language, temperature, clones).
        self.refresh_config();
        let entry = self
            .active_entry()
            .cloned()
            .ok_or_else(|| "no server model configured in ~/ttsd/server.yaml".to_string())?;
        // The tray writes the chosen voice into the daemon config, so the
        // config is the voice authority. Parrot's own voice argument is
        // only used when the config names no voice: its fallback logic is
        // built for Kokoro prefixes and would otherwise pick an arbitrary
        // voice for server models.
        let configured = self.default_voice();
        let voice = if configured.is_empty() { voice } else { configured.as_str() };
        let wav_bytes = self.synthesize_request(&entry, text, voice, speed)?;
        decode_wav(&wav_bytes)
    }
}

/// Decodes a WAV byte buffer into `SynthesisResult` samples.
fn decode_wav(bytes: &[u8]) -> Result<SynthesisResult, String> {
    let reader = WavReader::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("bad WAV from server: {e}"))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let samples: Vec<f32> = match spec.sample_format {
        SampleFormat::Float => reader
            .into_samples::<f32>()
            .map(|s| s.map_err(|e| e.to_string()))
            .collect::<Result<Vec<f32>, String>>()?,
        SampleFormat::Int => {
            let max = match spec.bits_per_sample {
                16 => i16::MAX as f32,
                24 => 8_388_607.0,
                32 => i32::MAX as f32,
                bits => {
                    return Err(format!("unsupported WAV bit depth: {bits}"));
                }
            };
            reader
                .into_samples::<i32>()
                .map(|s| s.map_err(|e| e.to_string()))
                .collect::<Result<Vec<i32>, String>>()?
                .into_iter()
                .map(|x| x as f32 / max)
                .collect()
        }
    };
    Ok(SynthesisResult {
        samples,
        sample_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_yields_no_models() {
        // Default config has no models — the family is simply absent.
        // (A real config exists on the dev machine; this test pins the
        // behaviour for CI where ~/ttsd does not exist.)
        if PathBuf::from(home_dir()).join(CONFIG_PATH).exists() {
            return; // dev machine: skip
        }
        assert!(list_server_models().is_empty());
    }

    #[test]
    fn decodes_int_wav_roundtrip() {
        // 1 sample of silence, 16-bit, 24 kHz.
        let mut buf = Vec::new();
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 24000,
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::new(std::io::Cursor::new(&mut buf), spec).unwrap();
            writer.write_sample(0i16).unwrap();
            writer.finalize().unwrap();
        }
        let result = decode_wav(&buf).unwrap();
        assert_eq!(result.sample_rate, 24000);
        assert_eq!(result.samples.len(), 1);
    }
}
