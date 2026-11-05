use crate::model_families::{self, LoadContext, SpeechEngine};
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use log::{debug, error, info, warn};
use rodio::buffer::SamplesBuffer;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player};
use serde::Serialize;
use std::num::NonZero;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager};

use crate::audio_feedback;
use crate::managers::history::HistoryManager;
use crate::managers::model::{EngineType, ModelManager};
use crate::settings::{get_settings, ModelUnloadTimeout, PlaybackMode};
use crate::text_normalization::normalize_text_for_tts;
use crate::utils::{
    hide_speaking_overlay, show_processing_overlay, show_processing_progress, show_speaking_overlay,
};

/// The built-in default model.
pub const MODEL_ID: &str = "kokoro";
const ENGINE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(2);
/// Crossfade length between text-level chunks. 10 ms matches the 240
/// samples tts-rs uses at 24 kHz; server models may run at other rates
/// (Ming: 44.1 kHz), so the sample count is derived per chunk.
const CROSSFADE_MS: u32 = 10;

fn crossfade_samples(sample_rate: u32) -> usize {
    (sample_rate as usize * CROSSFADE_MS as usize) / 1000
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelStateEvent {
    pub event_type: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub error: Option<String>,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TtsLifecycleState {
    Idle = 0,
    Processing = 1,
    Speaking = 2,
    Paused = 3,
}

struct ActiveSink {
    request_id: u64,
    sink: Arc<Player>,
}

pub struct TTSManager {
    engine: Arc<Mutex<Option<TtsEngine>>>,
    loaded_model_id: Arc<Mutex<Option<String>>>,
    app_handle: AppHandle,
    model_manager: Arc<ModelManager>,
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
    generation: Arc<AtomicU64>,
    active_request: Arc<AtomicU64>,
    lifecycle_state: Arc<AtomicU8>,
    current_sink: Arc<Mutex<Option<ActiveSink>>>,
    last_activity: Arc<AtomicU64>,
    shutdown_signal: Arc<AtomicBool>,
    espeak_ng_path: Option<PathBuf>,
    espeak_ng_data_path: Option<PathBuf>,
}

impl Drop for TTSManager {
    fn drop(&mut self) {
        self.shutdown_signal.store(true, Ordering::Relaxed);
    }
}

impl TTSManager {
    pub fn new(
        app_handle: &AppHandle,
        model_manager: Arc<ModelManager>,
        espeak_paths: (Option<PathBuf>, Option<PathBuf>),
    ) -> Result<Self> {
        let engine = Arc::new(Mutex::new(None));
        let is_loading = Arc::new(Mutex::new(false));
        let loading_condvar = Arc::new(Condvar::new());
        let generation = Arc::new(AtomicU64::new(0));
        let active_request = Arc::new(AtomicU64::new(0));
        let lifecycle_state = Arc::new(AtomicU8::new(TtsLifecycleState::Idle as u8));
        let current_sink = Arc::new(Mutex::new(None));
        let last_activity = Arc::new(AtomicU64::new(now_ms()));
        let shutdown_signal = Arc::new(AtomicBool::new(false));

        // Spawn idle watcher thread
        {
            let engine_clone = Arc::clone(&engine);
            let app_handle_clone = app_handle.clone();
            let last_activity_clone = Arc::clone(&last_activity);
            let shutdown_signal_clone = Arc::clone(&shutdown_signal);

            thread::spawn(move || {
                loop {
                    thread::sleep(Duration::from_secs(10));

                    if shutdown_signal_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    let settings = get_settings(&app_handle_clone);
                    let timeout_seconds = settings.model_unload_timeout.to_seconds();

                    if let Some(limit_seconds) = timeout_seconds {
                        // Immediate unloading happens when playback finishes or is cancelled
                        if settings.model_unload_timeout == ModelUnloadTimeout::Immediately {
                            continue;
                        }

                        let last = last_activity_clone.load(Ordering::Relaxed);
                        let now = now_ms();

                        if now.saturating_sub(last) > limit_seconds * 1000 {
                            let is_loaded = engine_is_loaded(&engine_clone);
                            if is_loaded {
                                debug!("Unloading TTS model due to inactivity");
                                *engine_clone.lock().unwrap() = None;
                                let _ = app_handle_clone.emit(
                                    "model-state-changed",
                                    ModelStateEvent {
                                        event_type: "unloaded".to_string(),
                                        model_id: None,
                                        model_name: None,
                                        error: None,
                                    },
                                );
                                info!("TTS model unloaded due to inactivity");
                            }
                        }
                    }
                }
                debug!("TTS idle watcher thread shutting down gracefully");
            });
        }

        Ok(Self {
            engine,
            loaded_model_id: Arc::new(Mutex::new(None)),
            app_handle: app_handle.clone(),
            model_manager,
            is_loading,
            loading_condvar,
            generation,
            active_request,
            lifecycle_state,
            current_sink,
            last_activity,
            shutdown_signal,
            espeak_ng_path: espeak_paths.0,
            espeak_ng_data_path: espeak_paths.1,
        })
    }

    /// Begins a new TTS request when idle.
    /// If a request is already active, this toggles it off and returns `None`.
    pub fn begin_request_or_toggle_stop(&self) -> Option<u64> {
        if self.active_request.load(Ordering::SeqCst) != 0 {
            self.stop();
            return None;
        }

        let request_id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.active_request.store(request_id, Ordering::SeqCst);
        self.set_lifecycle_state(TtsLifecycleState::Processing);
        Some(request_id)
    }

    pub fn is_request_active(&self, request_id: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == request_id
            && self.active_request.load(Ordering::SeqCst) == request_id
    }

    pub fn stop_if_request_active(&self, request_id: u64) -> bool {
        let is_active_request = self.active_request.load(Ordering::SeqCst) == request_id;
        if !is_active_request {
            return false;
        }

        if self
            .generation
            .compare_exchange(
                request_id,
                request_id.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return false;
        }

        clear_active_request_if_owned(&self.active_request, request_id);
        self.transition_to_idle_and_stop_sink();
        debug!("TTS playback stopped for request {}", request_id);
        true
    }

    /// Kick off model loading in the background. No-op if already loading or loaded.
    pub fn initiate_model_load(&self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading || engine_is_loaded(&self.engine) {
            return;
        }
        *is_loading = true;

        let engine = Arc::clone(&self.engine);
        let is_loading_arc = Arc::clone(&self.is_loading);
        let condvar = Arc::clone(&self.loading_condvar);
        let app_handle = self.app_handle.clone();
        let model_manager = Arc::clone(&self.model_manager);
        let loaded_model_id = Arc::clone(&self.loaded_model_id);
        let model_id = selected_model_id(&self.app_handle);
        let espeak_ng_path = self.espeak_ng_path.clone();
        let espeak_ng_data_path = self.espeak_ng_data_path.clone();

        thread::spawn(move || {
            // Resolve human-readable name from ModelManager; fall back to ID if missing.
            let model_info = model_manager.get_model_info(&model_id);
            let model_name = model_info
                .as_ref()
                .map(|info| info.name.clone())
                .unwrap_or_else(|| model_id.clone());
            let engine_type = model_info
                .as_ref()
                .map(|info| info.engine_type.clone())
                .unwrap_or(EngineType::Kokoro);

            let model_dir = match model_info
                .as_ref()
                .filter(|_| model_id != MODEL_ID)
                .map(|info| Ok(model_manager.model_dir(info)))
                .unwrap_or_else(|| resolve_kokoro_model_dir(&app_handle))
            {
                Ok(dir) => dir,
                Err(e) => {
                    error!("{}", e);
                    let _ = app_handle.emit("tts-error", e.clone());
                    *is_loading_arc.lock().unwrap() = false;
                    condvar.notify_all();
                    return;
                }
            };

            info!("Loading {} model from {:?}", model_name, model_dir);
            let _ = app_handle.emit(
                "model-state-changed",
                ModelStateEvent {
                    event_type: "loading_started".to_string(),
                    model_id: Some(model_id.clone()),
                    model_name: Some(model_name.clone()),
                    error: None,
                },
            );
            let num_threads = infer_kokoro_thread_count();
            info!("{} tuning: num_threads={}", model_name, num_threads);
            let cache_dir = model_info.as_ref().and_then(|info| {
                let dir = model_manager.model_dir(info);
                std::fs::create_dir_all(&dir).ok()?;
                Some(dir)
            });
            let load_result = model_families::family_for(&engine_type).load(&LoadContext {
                model_dir: &model_dir,
                cache_dir,
                espeak_bin: espeak_ng_path.clone(),
                espeak_data: espeak_ng_data_path.clone(),
                num_threads,
            });

            let load_error = match load_result {
                Err(e) => {
                    let engine_error = format!(
                        "Failed to load {} from {}: {}",
                        model_name,
                        model_dir.display(),
                        e
                    );
                    error!("{}", engine_error);
                    *engine.lock().unwrap() = None;
                    *loaded_model_id.lock().unwrap() = None;
                    Some(engine_error)
                }
                Ok(mut loaded) => {
                    info!("Warming up {} pipeline (espeak-ng + ORT)...", model_name);
                    let warmup_voice = loaded.default_voice();
                    let _ = loaded.synthesize("Hello.", &warmup_voice, 0, 1.0);
                    *engine.lock().unwrap() = Some(loaded);
                    *loaded_model_id.lock().unwrap() = Some(model_id.clone());
                    None
                }
            };

            if let Some(error_msg) = load_error {
                hide_speaking_overlay(&app_handle);
                let _ = app_handle.emit(
                    "model-state-changed",
                    ModelStateEvent {
                        event_type: "loading_failed".to_string(),
                        model_id: Some(model_id.clone()),
                        model_name: Some(model_name),
                        error: Some(error_msg.clone()),
                    },
                );
                let _ = app_handle.emit("tts-error", error_msg);
            } else {
                info!("{} model loaded successfully", model_name);
                let _ = app_handle.emit(
                    "model-state-changed",
                    ModelStateEvent {
                        event_type: "loaded".to_string(),
                        model_id: Some(model_id.clone()),
                        model_name: Some(model_name),
                        error: None,
                    },
                );
            }
            *is_loading_arc.lock().unwrap() = false;
            condvar.notify_all();
        });
    }

    /// Stop any in-progress TTS playback or processing.
    pub fn stop(&self) {
        let request_id = self.active_request.swap(0, Ordering::SeqCst);
        if request_id != 0 {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        self.transition_to_idle_and_stop_sink();
        debug!("TTS playback stopped");
    }

    pub fn is_model_loaded(&self) -> bool {
        engine_is_loaded(&self.engine)
    }

    pub fn is_model_loading(&self) -> bool {
        *self.is_loading.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Returns all available Kokoro voices in sorted order.
    /// Triggers loading when needed and waits for the load cycle to finish.
    pub fn get_available_voices(&self) -> Result<Vec<String>> {
        if !self.is_model_loaded() {
            self.initiate_model_load();
            self.wait_for_pending_model_load()?;
        }

        if !engine_is_loaded(&self.engine) {
            return Err(anyhow!(
                "TTS model is not loaded. Install the model files, or start the speech server (ttsd), and try again."
            ));
        }

        let voices = collect_available_voices(&self.engine);

        if voices.is_empty() {
            return Err(anyhow!("No Kokoro voices were found in the loaded model."));
        }

        Ok(voices)
    }

    /// Returns the loaded model's id, `None` when nothing is loaded.
    pub fn get_current_model(&self) -> Option<String> {
        if self.is_model_loaded() {
            self.loaded_model_id.lock().unwrap().clone()
        } else {
            None
        }
    }

    /// Unload the Kokoro engine from memory and emit a model-state-changed event.
    pub fn unload_model(&self) -> Result<()> {
        debug!("Unloading TTS model");
        *self.engine.lock().unwrap() = None;
        *self.loaded_model_id.lock().unwrap() = None;
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "unloaded".to_string(),
                model_id: None,
                model_name: None,
                error: None,
            },
        );
        info!("TTS model unloaded");
        Ok(())
    }

    /// Unloads the model immediately if the ModelUnloadTimeout setting is set to Immediately.
    pub fn maybe_unload_immediately(&self, context: &str) {
        let settings = get_settings(&self.app_handle);
        if settings.model_unload_timeout == ModelUnloadTimeout::Immediately
            && self.is_model_loaded()
        {
            info!("Immediately unloading TTS model after {}", context);
            if let Err(e) = self.unload_model() {
                warn!("Failed to immediately unload TTS model: {}", e);
            }
        }
    }

    /// Pause/resume active speech.
    /// Returns `true` when playback is paused, `false` when resumed.
    pub fn toggle_pause(&self) -> Result<bool> {
        let sink_guard = self.current_sink.lock().unwrap();
        let Some(active_sink) = sink_guard.as_ref() else {
            return Err(anyhow!("No active TTS playback"));
        };
        let player = &active_sink.sink;

        match self.current_lifecycle_state() {
            TtsLifecycleState::Speaking => {
                player.pause();
                self.set_lifecycle_state(TtsLifecycleState::Paused);
                Ok(true)
            }
            TtsLifecycleState::Paused => {
                player.play();
                self.set_lifecycle_state(TtsLifecycleState::Speaking);
                Ok(false)
            }
            _ => Err(anyhow!("TTS playback is not in a pausable state")),
        }
    }

    /// Synthesize and play for a previously-started request.
    pub fn speak(&self, text: String, request_id: u64) {
        // Record activity timestamp for idle watcher
        self.last_activity.store(now_ms(), Ordering::Relaxed);

        let engine = Arc::clone(&self.engine);
        let is_loading = Arc::clone(&self.is_loading);
        let condvar = Arc::clone(&self.loading_condvar);
        let generation = Arc::clone(&self.generation);
        let active_request = Arc::clone(&self.active_request);
        let lifecycle_state = Arc::clone(&self.lifecycle_state);
        let current_sink = Arc::clone(&self.current_sink);
        let app_handle = self.app_handle.clone();

        thread::spawn(move || {
            let started_at = Instant::now();

            {
                let mut loading = is_loading.lock().unwrap();
                while *loading {
                    loading = condvar.wait(loading).unwrap();
                }
            }

            if generation.load(Ordering::SeqCst) != request_id
                || active_request.load(Ordering::SeqCst) != request_id
            {
                return;
            }

            if !engine_is_loaded(&engine) {
                let message = "TTS model is not loaded. Install the model files, or start the speech server (ttsd), and try again.";
                error!("{}", message);
                if set_idle_and_cleanup_for_request(
                    &generation,
                    &active_request,
                    request_id,
                    &lifecycle_state,
                    &current_sink,
                    &app_handle,
                ) {
                    let _ = app_handle.emit("tts-error", message);
                }
                return;
            }

            // Show "Processing..." overlay immediately so the user gets feedback
            // before the first audio chunk is synthesized.
            show_processing_overlay(&app_handle);

            let tts_settings = get_settings(&app_handle);
            let tts_speed = tts_settings.tts_speed;
            let playback_mode = tts_settings.tts_playback_mode;
            // A short first chunk only helps when playback starts after it.
            let shorten_first_chunk = tts_settings.tts_shorten_first_chunk
                && playback_mode == PlaybackMode::Stream;
            let normalized_text = normalize_text_for_tts(&text);

            debug!(
                "Prepared {} input chars into {} chars for TTS",
                text.len(),
                normalized_text.len()
            );

            let chunks = split_text_for_playback(&normalized_text, shorten_first_chunk);
            if chunks.is_empty() {
                let message = "No readable text found for TTS.";
                error!("{}", message);
                if set_idle_and_cleanup_for_request(
                    &generation,
                    &active_request,
                    request_id,
                    &lifecycle_state,
                    &current_sink,
                    &app_handle,
                ) {
                    let _ = app_handle.emit("tts-error", message);
                }
                return;
            }

            let _stream = match open_output_stream(&app_handle) {
                Ok(s) => s,
                Err(e) => {
                    error!("{}", e);
                    if set_idle_and_cleanup_for_request(
                        &generation,
                        &active_request,
                        request_id,
                        &lifecycle_state,
                        &current_sink,
                        &app_handle,
                    ) {
                        let _ = app_handle.emit("tts-error", e);
                    }
                    return;
                }
            };

            let player = Player::connect_new(_stream.mixer());
            player.pause();
            let shared_sink = Arc::new(player);

            {
                let mut sink_guard = current_sink.lock().unwrap();
                if generation.load(Ordering::SeqCst) != request_id
                    || active_request.load(Ordering::SeqCst) != request_id
                {
                    return;
                }
                *sink_guard = Some(ActiveSink {
                    request_id,
                    sink: shared_sink.clone(),
                });
            }

            if !engine_is_loaded(&engine) {
                let message = "TTS model is not loaded. Install the model files, or start the speech server (ttsd), and try again.";
                error!("{}", message);
                if set_idle_and_cleanup_for_request(
                    &generation,
                    &active_request,
                    request_id,
                    &lifecycle_state,
                    &current_sink,
                    &app_handle,
                ) {
                    let _ = app_handle.emit("tts-error", message);
                }
                return;
            }

            let chunks = Arc::new(chunks);
            let total_chunks = chunks.len();
            let total_chars: usize = chunks.iter().map(|c| c.len()).sum();
            let shared_style_index = estimate_kokoro_style_index(chunks.as_ref());
            let resolved_language: Option<String> = if tts_settings.selected_language == "auto" {
                tauri_plugin_os::locale().map(|l| l.replace('_', "-"))
            } else {
                Some(tts_settings.selected_language.clone())
            };
            let selected_voice_override = tts_settings.selected_kokoro_voice.as_deref();
            let available_voices = collect_available_voices(&engine);
            let selected_voice = select_kokoro_voice_for_text(
                resolved_language.as_deref(),
                selected_voice_override,
                &available_voices,
            );
            let mut total_synth_secs = 0.0_f32;
            let mut started_playback = false;
            let mut buffered_seconds = 0.0_f32;
            let mut total_audio_seconds = 0.0_f32;
            debug!(
                "TTS split into {} chunks ({} chars) (style_index={}, voice={})",
                total_chunks, total_chars, shared_style_index, selected_voice
            );

            let mut collected_samples: Vec<f32> = Vec::new();
            let mut collected_sample_rate: u32 = 24000;
            let mut crossfade_tail: Option<Vec<f32>> = None;
            // Channel for sending (chunk_index, duration_secs, level envelope)
            // to the overlay updater thread as chunks are appended to the sink.
            let (chunk_dur_tx, chunk_dur_rx) = mpsc::channel::<ChunkTiming>();
            let mut chunk_dur_rx = Some(chunk_dur_rx);

            for (chunk_index, chunk) in chunks.iter().enumerate() {
                if !request_is_active(&generation, &active_request, request_id) {
                    shared_sink.stop();
                    clear_active_request_if_owned(&active_request, request_id);
                    clear_sink_if_owned_by_request(&current_sink, request_id);
                    return;
                }

                let synth_start = Instant::now();

                // Take the engine out of its slot so the mutex is released
                // during the (potentially long) synthesize() call. This lets
                // new requests acquire the engine immediately after cancel.
                let mut kokoro_engine = match take_engine_for_active_request(
                    &engine,
                    &generation,
                    &active_request,
                    request_id,
                ) {
                    Some(e) => e,
                    None => {
                        clear_active_request_if_owned(&active_request, request_id);
                        clear_sink_if_owned_by_request(&current_sink, request_id);
                        return;
                    }
                };

                if !request_is_active(&generation, &active_request, request_id) {
                    return_engine_to_slot(&engine, kokoro_engine);
                    clear_active_request_if_owned(&active_request, request_id);
                    clear_sink_if_owned_by_request(&current_sink, request_id);
                    return;
                }

                // Synthesize WITHOUT holding the mutex lock.
                let synth_result = kokoro_engine
                    .synthesize(chunk, &selected_voice, shared_style_index, tts_speed)
                    .map_err(|e| format!("TTS synthesis failed: {}", e));

                // Put engine back (brief lock, just a swap).
                return_engine_to_slot(&engine, kokoro_engine);

                let synth_elapsed_secs = synth_start.elapsed().as_secs_f32();

                if !request_is_active(&generation, &active_request, request_id) {
                    shared_sink.stop();
                    clear_active_request_if_owned(&active_request, request_id);
                    clear_sink_if_owned_by_request(&current_sink, request_id);
                    return;
                }

                let synthesis = match synth_result {
                    Ok(synthesis) => synthesis,
                    Err(err) => {
                        error!("{}", err);
                        if set_idle_and_cleanup_for_request(
                            &generation,
                            &active_request,
                            request_id,
                            &lifecycle_state,
                            &current_sink,
                            &app_handle,
                        ) {
                            let _ = app_handle.emit("tts-error", err);
                        }
                        return;
                    }
                };

                total_synth_secs += synth_elapsed_secs;

                if synthesis.samples.is_empty() {
                    continue;
                }

                let chunk_audio_seconds =
                    synthesis.samples.len() as f32 / synthesis.sample_rate as f32;
                total_audio_seconds += chunk_audio_seconds;
                buffered_seconds += chunk_audio_seconds;
                collected_sample_rate = synthesis.sample_rate;
                collected_samples.extend_from_slice(&synthesis.samples);

                // Crossfade with the previous chunk's tail to eliminate clicks
                // at chunk boundaries (10ms @ 24kHz = 240 samples).
                let mut samples = synthesis.samples;
                if let Some(prev_tail) = crossfade_tail.take() {
                    apply_crossfade(&prev_tail, &mut samples);
                }
                // Hold back the last crossfade window for blending with the next chunk
                let crossfade_len = crossfade_samples(synthesis.sample_rate);
                if samples.len() > crossfade_len {
                    let split = samples.len() - crossfade_len;
                    crossfade_tail = Some(samples[split..].to_vec());
                    samples.truncate(split);
                }
                // Use the post-trim sample count (what's actually appended to
                // the sink now) rather than chunk_audio_seconds (pre-trim) so
                // the overlay text updater's sleep timing doesn't drift ahead
                // of real playback by CROSSFADE_SAMPLES on every chunk.
                let appended_audio_seconds = samples.len() as f32 / synthesis.sample_rate as f32;
                let levels = level_envelope(&samples, synthesis.sample_rate);
                shared_sink.append(SamplesBuffer::new(
                    NonZero::new(1u16).unwrap(),
                    NonZero::new(synthesis.sample_rate).unwrap(),
                    samples,
                ));
                // Feed the overlay updater thread so it can schedule when to
                // show each chunk's text and animate its level bars.
                let _ = chunk_dur_tx.send(ChunkTiming {
                    index: chunk_index,
                    duration_secs: appended_audio_seconds,
                    levels,
                });

                // Report render progress while playback has not started yet
                // (buffered and complete modes hold the audio back).
                if !started_playback && playback_mode != PlaybackMode::Stream {
                    show_processing_progress(&app_handle, chunk_index + 1, total_chunks);
                }

                let chars_done: usize = chunks[..=chunk_index].iter().map(|c| c.len()).sum();
                let next_chunk_chars = chunks.get(chunk_index + 1).map_or(0, |c| c.len());
                let ready_to_play = playback_start_reached(
                    playback_mode,
                    chunk_index + 1 == total_chunks,
                    buffered_seconds,
                    total_synth_secs,
                    chars_done,
                    total_chars,
                    next_chunk_chars,
                );

                if !started_playback && ready_to_play {
                    if !request_is_active(&generation, &active_request, request_id) {
                        shared_sink.stop();
                        clear_active_request_if_owned(&active_request, request_id);
                        clear_sink_if_owned_by_request(&current_sink, request_id);
                        return;
                    }
                    started_playback = true;
                    lifecycle_state.store(TtsLifecycleState::Speaking as u8, Ordering::SeqCst);
                    show_speaking_overlay(&app_handle, chunks.first().cloned());
                    crate::shortcut::register_play_pause_shortcut(&app_handle);
                    audio_feedback::play_sound(&app_handle, audio_feedback::SoundType::Start);
                    shared_sink.play();

                    // Spawn overlay text updater: sleeps through each chunk's
                    // audio duration and emits the next chunk's text at the
                    // right moment, pausing the timer when TTS is paused.
                    if let Some(rx) = chunk_dur_rx.take() {
                        let overlay_chunks = Arc::clone(&chunks);
                        let overlay_lifecycle = Arc::clone(&lifecycle_state);
                        let overlay_generation = Arc::clone(&generation);
                        let overlay_app = app_handle.clone();
                        thread::spawn(move || {
                            overlay_text_updater(
                                rx,
                                &overlay_chunks,
                                &overlay_lifecycle,
                                &overlay_generation,
                                request_id,
                                &overlay_app,
                            );
                        });
                    }

                    let overall_rtf = total_synth_secs / buffered_seconds.max(0.001);
                    info!(
                        "TTS playback started in {}ms (buffered={:.2}s, rtf={:.2}, chunks={}/{})",
                        started_at.elapsed().as_millis(),
                        buffered_seconds,
                        overall_rtf,
                        chunk_index + 1,
                        total_chunks,
                    );
                }

                debug!(
                    "TTS chunk {}/{} synthesized in {}ms ({:.2}s audio, rtf={:.2})",
                    chunk_index + 1,
                    total_chunks,
                    (synth_elapsed_secs * 1000.0) as u64,
                    chunk_audio_seconds,
                    synth_elapsed_secs / chunk_audio_seconds.max(0.001),
                );
            }

            // Drop the sender so the overlay updater thread knows no more chunks are coming.
            drop(chunk_dur_tx);

            // Buffered/complete mode with all chunks rendered before the
            // start condition fired (or a single chunk): start now.
            if !started_playback && total_audio_seconds > 0.0 {
                if !request_is_active(&generation, &active_request, request_id) {
                    shared_sink.stop();
                    clear_active_request_if_owned(&active_request, request_id);
                    clear_sink_if_owned_by_request(&current_sink, request_id);
                    return;
                }
                started_playback = true;
                lifecycle_state.store(TtsLifecycleState::Speaking as u8, Ordering::SeqCst);
                show_speaking_overlay(&app_handle, chunks.first().cloned());
                crate::shortcut::register_play_pause_shortcut(&app_handle);
                audio_feedback::play_sound(&app_handle, audio_feedback::SoundType::Start);
                shared_sink.play();
                if let Some(rx) = chunk_dur_rx.take() {
                    let overlay_chunks = Arc::clone(&chunks);
                    let overlay_lifecycle = Arc::clone(&lifecycle_state);
                    let overlay_generation = Arc::clone(&generation);
                    let overlay_app = app_handle.clone();
                    thread::spawn(move || {
                        overlay_text_updater(
                            rx,
                            &overlay_chunks,
                            &overlay_lifecycle,
                            &overlay_generation,
                            request_id,
                            &overlay_app,
                        );
                    });
                }
                info!(
                    "TTS playback started in {}ms after full render (mode={:?}, {:.2}s audio, rtf={:.2})",
                    started_at.elapsed().as_millis(),
                    playback_mode,
                    total_audio_seconds,
                    total_synth_secs / total_audio_seconds.max(0.001),
                );
            }

            // Flush any held-back crossfade tail from the final chunk
            if let Some(tail) = crossfade_tail.take() {
                shared_sink.append(SamplesBuffer::new(
                    NonZero::new(1u16).unwrap(),
                    NonZero::new(collected_sample_rate).unwrap(),
                    tail,
                ));
            }

            if !started_playback {
                let message = "TTS generated empty audio output.";
                error!("{}", message);
                if set_idle_and_cleanup_for_request(
                    &generation,
                    &active_request,
                    request_id,
                    &lifecycle_state,
                    &current_sink,
                    &app_handle,
                ) {
                    let _ = app_handle.emit("tts-error", message);
                }
                return;
            }

            while !shared_sink.empty() {
                if generation.load(Ordering::SeqCst) != request_id
                    || active_request.load(Ordering::SeqCst) != request_id
                {
                    shared_sink.stop();
                    clear_active_request_if_owned(&active_request, request_id);
                    clear_sink_if_owned_by_request(&current_sink, request_id);
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }

            if generation.load(Ordering::SeqCst) == request_id
                && active_request.load(Ordering::SeqCst) == request_id
            {
                clear_active_request_if_owned(&active_request, request_id);
                lifecycle_state.store(TtsLifecycleState::Idle as u8, Ordering::SeqCst);
                clear_sink_if_owned_by_request(&current_sink, request_id);
                hide_speaking_overlay(&app_handle);
                crate::shortcut::unregister_play_pause_shortcut(&app_handle);
                audio_feedback::play_sound(&app_handle, audio_feedback::SoundType::Stop);
                info!(
                    "TTS playback finished in {}ms ({:.2}s audio)",
                    started_at.elapsed().as_millis(),
                    total_audio_seconds
                );

                // Save to history
                if !collected_samples.is_empty() {
                    let history_manager = app_handle.state::<Arc<HistoryManager>>();
                    if let Err(e) = history_manager.save_synthesis(
                        collected_samples,
                        text.clone(),
                        collected_sample_rate,
                    ) {
                        error!("Failed to save TTS history entry: {}", e);
                    }
                }

                if let Some(manager) = app_handle.try_state::<Arc<TTSManager>>() {
                    manager.maybe_unload_immediately("playback");
                }
            } else {
                clear_active_request_if_owned(&active_request, request_id);
                clear_sink_if_owned_by_request(&current_sink, request_id);
            }
        });
    }

    fn current_lifecycle_state(&self) -> TtsLifecycleState {
        match self.lifecycle_state.load(Ordering::SeqCst) {
            1 => TtsLifecycleState::Processing,
            2 => TtsLifecycleState::Speaking,
            3 => TtsLifecycleState::Paused,
            _ => TtsLifecycleState::Idle,
        }
    }

    fn set_lifecycle_state(&self, state: TtsLifecycleState) {
        self.lifecycle_state.store(state as u8, Ordering::SeqCst);
    }

    fn transition_to_idle_and_stop_sink(&self) {
        self.active_request.store(0, Ordering::SeqCst);
        self.set_lifecycle_state(TtsLifecycleState::Idle);

        if let Ok(mut sink_guard) = self.current_sink.lock() {
            if let Some(active_sink) = sink_guard.take() {
                active_sink.sink.stop();
            }
        }

        hide_speaking_overlay(&self.app_handle);
    }

    fn wait_for_pending_model_load(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut loading = self.is_loading.lock().unwrap_or_else(|p| p.into_inner());
        while *loading {
            let now = Instant::now();
            if now >= deadline {
                return Err(anyhow!(
                    "Timed out while waiting for Kokoro model loading to finish."
                ));
            }

            let remaining = deadline.saturating_duration_since(now);
            let (next_loading, wait_result) = self
                .loading_condvar
                .wait_timeout(loading, remaining)
                .unwrap_or_else(|p| p.into_inner());
            loading = next_loading;

            if wait_result.timed_out() && *loading {
                return Err(anyhow!(
                    "Timed out while waiting for Kokoro model loading to finish."
                ));
            }
        }

        Ok(())
    }
}

/// Runs on a dedicated thread. Receives `(chunk_index, duration_secs)` from the
/// synthesis loop and emits `overlay-text` events timed to when each chunk
/// actually starts playing, so the overlay shows the text being read aloud.
///
/// The timer pauses when TTS playback is paused and exits when the request
/// is cancelled or all chunks have been emitted.
fn overlay_text_updater(
    rx: mpsc::Receiver<ChunkTiming>,
    chunks: &[String],
    lifecycle_state: &AtomicU8,
    generation: &AtomicU64,
    request_id: u64,
    app_handle: &AppHandle,
) {
    // Collect chunk durations as they arrive from the synthesis loop.
    // We emit each chunk's text *before* sleeping through its duration,
    // which correctly handles both the normal case (chunks pre-buffered)
    // and the "gap" case (synthesis lagging behind playback).
    let mut pending: Vec<ChunkTiming> = Vec::new();
    let mut current: usize = 0; // index of the chunk we're about to sleep through

    // Chunk 0's text is already shown by show_speaking_overlay.

    loop {
        if generation.load(Ordering::SeqCst) != request_id {
            return;
        }

        // Drain any newly arrived chunk durations
        while let Ok(entry) = rx.try_recv() {
            pending.push(entry);
        }

        // Wait until we have timing info for the current chunk
        if current >= pending.len() {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(entry) => pending.push(entry),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if current >= pending.len() {
                continue;
            }
        }

        // Emit the current chunk's text. Chunk 0 is already displayed by
        // show_speaking_overlay; for subsequent chunks this fires right
        // when the chunk starts playing — whether it was pre-buffered or
        // arrived after a synthesis gap.
        if current > 0 {
            let chunk_idx = pending[current].index;
            if let Some(text) = chunks.get(chunk_idx) {
                let _ = app_handle.emit("overlay-text", text.as_str());
            }
        }

        // Sleep through the current chunk's audio duration, emitting the
        // level of the window that is playing right now so the overlay's
        // bars follow the real audio instead of a decorative loop.
        let sleep_target = Duration::from_secs_f32(pending[current].duration_secs);
        let mut slept = Duration::ZERO;

        while slept < sleep_target {
            if generation.load(Ordering::SeqCst) != request_id {
                return;
            }

            let state = lifecycle_state.load(Ordering::SeqCst);
            if state == TtsLifecycleState::Paused as u8 {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            if state != TtsLifecycleState::Speaking as u8 {
                return;
            }

            let window = (slept.as_millis() / LEVEL_WINDOW_MS as u128) as usize;
            if let Some(level) = pending[current].levels.get(window) {
                let _ = app_handle.emit("overlay-level", *level);
            }

            let step = Duration::from_millis(50).min(sleep_target - slept);
            thread::sleep(step);
            slept += step;
        }

        current += 1;
    }
}

/// Length of one level window sent to the overlay (matches the updater's
/// 50 ms sleep step).
const LEVEL_WINDOW_MS: u32 = 50;

/// One synthesized chunk as seen by the overlay updater.
struct ChunkTiming {
    index: usize,
    duration_secs: f32,
    /// Normalized RMS level (0.0..=1.0) per LEVEL_WINDOW_MS window.
    levels: Vec<f32>,
}

/// RMS level per LEVEL_WINDOW_MS window, scaled so ordinary speech fills
/// most of the bar height (speech RMS sits around 0.05..0.2).
fn level_envelope(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    let window = ((sample_rate as usize * LEVEL_WINDOW_MS as usize) / 1000).max(1);
    samples
        .chunks(window)
        .map(|w| {
            let mean_sq = w.iter().map(|s| s * s).sum::<f32>() / w.len() as f32;
            (mean_sq.sqrt() * 4.0).min(1.0)
        })
        .collect()
}

/// Linear crossfade: blend `prev_tail` into the beginning of `samples`.
/// If `samples` is shorter than `prev_tail`, only the overlapping portion is blended
/// and the non-overlapping prefix of `prev_tail` is prepended.
fn apply_crossfade(prev_tail: &[f32], samples: &mut Vec<f32>) {
    let overlap = prev_tail.len().min(samples.len());
    for i in 0..overlap {
        let t = (i + 1) as f32 / (overlap + 1) as f32;
        samples[i] = prev_tail[prev_tail.len() - overlap + i] * (1.0 - t) + samples[i] * t;
    }
    if prev_tail.len() > overlap {
        let prefix = &prev_tail[..prev_tail.len() - overlap];
        samples.splice(0..0, prefix.iter().copied());
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn infer_kokoro_thread_count() -> usize {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    infer_kokoro_thread_count_for_cpu_count(cpu_count)
}

fn infer_kokoro_thread_count_for_cpu_count(cpu_count: usize) -> usize {
    // Keep one core free on larger systems so UI/audio and background tasks remain smooth.
    let reserved_cores = if cpu_count >= 4 { 1 } else { 0 };
    cpu_count.saturating_sub(reserved_cores).max(1)
}

pub type TtsEngine = Box<dyn SpeechEngine>;

fn engine_is_loaded(engine: &Arc<Mutex<Option<TtsEngine>>>) -> bool {
    engine.lock().map(|guard| guard.is_some()).unwrap_or(false)
}

fn collect_available_voices(engine: &Arc<Mutex<Option<TtsEngine>>>) -> Vec<String> {
    let mut voices = Vec::new();

    let guard = engine.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(engine) = guard.as_ref() {
        voices.extend(engine.list_voices());
    }
    drop(guard);

    voices.sort_unstable();
    voices.dedup();
    voices.retain(|voice| !voice.trim().is_empty());
    voices
}

fn request_is_active(
    generation: &Arc<AtomicU64>,
    active_request: &Arc<AtomicU64>,
    request_id: u64,
) -> bool {
    generation.load(Ordering::SeqCst) == request_id
        && active_request.load(Ordering::SeqCst) == request_id
}

/// Take the engine out of its slot so synthesis can proceed without holding the mutex.
/// Returns `None` if the request was cancelled while waiting.
/// The caller is responsible for putting the engine back after synthesis.
fn take_engine_for_active_request(
    engine_slot: &Arc<Mutex<Option<TtsEngine>>>,
    generation: &Arc<AtomicU64>,
    active_request: &Arc<AtomicU64>,
    request_id: u64,
) -> Option<TtsEngine> {
    loop {
        if !request_is_active(generation, active_request, request_id) {
            return None;
        }

        match engine_slot.try_lock() {
            Ok(mut guard) => {
                if let Some(engine) = guard.take() {
                    return Some(engine);
                }
                // Engine was taken by another worker; wait for it to be put back.
                drop(guard);
                thread::sleep(ENGINE_LOCK_POLL_INTERVAL);
            }
            Err(TryLockError::WouldBlock) => thread::sleep(ENGINE_LOCK_POLL_INTERVAL),
            Err(TryLockError::Poisoned(poisoned)) => return poisoned.into_inner().take(),
        }
    }
}

/// Put the engine back into its slot after synthesis.
fn return_engine_to_slot(engine_slot: &Arc<Mutex<Option<TtsEngine>>>, engine: TtsEngine) {
    match engine_slot.lock() {
        Ok(mut guard) => *guard = Some(engine),
        Err(poisoned) => *poisoned.into_inner() = Some(engine),
    }
}

fn set_idle_and_cleanup_for_request(
    generation: &Arc<AtomicU64>,
    active_request: &Arc<AtomicU64>,
    request_id: u64,
    lifecycle_state: &Arc<AtomicU8>,
    current_sink: &Arc<Mutex<Option<ActiveSink>>>,
    app_handle: &AppHandle,
) -> bool {
    if generation.load(Ordering::SeqCst) != request_id
        || active_request.load(Ordering::SeqCst) != request_id
    {
        clear_active_request_if_owned(active_request, request_id);
        clear_sink_if_owned_by_request(current_sink, request_id);
        return false;
    }

    clear_active_request_if_owned(active_request, request_id);
    lifecycle_state.store(TtsLifecycleState::Idle as u8, Ordering::SeqCst);
    clear_sink_if_owned_by_request(current_sink, request_id);
    hide_speaking_overlay(app_handle);
    true
}

fn clear_active_request_if_owned(active_request: &Arc<AtomicU64>, request_id: u64) {
    let _ = active_request.compare_exchange(request_id, 0, Ordering::SeqCst, Ordering::SeqCst);
}

fn estimate_kokoro_style_index(chunks: &[String]) -> usize {
    chunks
        .iter()
        .map(|chunk| chunk.chars().filter(|ch| !ch.is_whitespace()).count())
        .sum::<usize>()
        .max(1)
}

fn select_kokoro_voice_for_text(
    language_hint: Option<&str>,
    selected_voice_override: Option<&str>,
    available_voices: &[String],
) -> String {
    if let Some(preferred_voice) =
        resolve_preferred_voice_selection(selected_voice_override, available_voices)
    {
        return preferred_voice;
    }

    let preferred_language = normalize_kokoro_language_code(language_hint).unwrap_or("en-us");

    select_voice_for_language(preferred_language, available_voices)
}

fn resolve_preferred_voice_selection(
    preferred_voice: Option<&str>,
    available_voices: &[String],
) -> Option<String> {
    let preferred = preferred_voice?.trim();
    if preferred.is_empty() {
        return None;
    }

    available_voices
        .iter()
        .find(|voice| voice.eq_ignore_ascii_case(preferred))
        .cloned()
}

fn select_voice_for_language(language: &str, available_voices: &[String]) -> String {
    let mut canonical_voices: Vec<&str> = available_voices
        .iter()
        .filter_map(|voice| {
            let trimmed = voice.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .collect();
    canonical_voices.sort_unstable();
    canonical_voices.dedup();

    let prefixes = voice_prefixes_for_language(language);
    let fallback_prefixes = voice_prefixes_for_language("en-us");

    for prefix in prefixes {
        if let Some(voice) = canonical_voices
            .iter()
            .find(|voice| voice.starts_with(prefix))
            .copied()
        {
            return voice.to_string();
        }
    }

    for prefix in fallback_prefixes {
        if let Some(voice) = canonical_voices
            .iter()
            .find(|voice| voice.starts_with(prefix))
            .copied()
        {
            return voice.to_string();
        }
    }

    canonical_voices
        .first()
        .map(|voice| voice.to_string())
        .unwrap_or_else(|| "af_heart".to_string())
}

fn voice_prefixes_for_language(language: &str) -> &'static [&'static str] {
    match language {
        "en-gb" => &["bf_", "bm_"],
        "es" => &["ef_", "em_"],
        "fr" => &["ff_"],
        "hi" => &["hf_", "hm_"],
        "it" => &["if_", "im_"],
        "ja" => &["jf_", "jm_"],
        "pt-br" => &["pf_", "pm_"],
        "cmn" => &["zf_", "zm_"],
        _ => &["af_", "am_"],
    }
}

fn normalize_kokoro_language_code(language: Option<&str>) -> Option<&'static str> {
    let raw = language?.trim();
    if raw.is_empty() {
        return None;
    }

    let normalized = raw.to_ascii_lowercase().replace('_', "-");
    let base = normalized.split('-').next().unwrap_or_default();

    if matches!(normalized.as_str(), "en-gb" | "en-uk") {
        return Some("en-gb");
    }
    if matches!(
        normalized.as_str(),
        "zh-hans"
            | "zh-hant"
            | "zh-cn"
            | "zh-tw"
            | "zh-hk"
            | "cmn"
            | "cmn-hans"
            | "cmn-hant"
            | "yue"
            | "yue-hk"
    ) {
        return Some("cmn");
    }
    if normalized == "pt-br" {
        return Some("pt-br");
    }

    match base {
        "en" => Some("en-us"),
        "es" => Some("es"),
        "fr" => Some("fr"),
        "hi" => Some("hi"),
        "it" => Some("it"),
        "ja" | "jp" => Some("ja"),
        "pt" => Some("pt-br"),
        "zh" => Some("cmn"),
        _ => None,
    }
}

fn clear_sink_if_owned_by_request(current_sink: &Arc<Mutex<Option<ActiveSink>>>, request_id: u64) {
    if let Ok(mut sink_guard) = current_sink.lock() {
        let is_owned = sink_guard
            .as_ref()
            .map(|active_sink| active_sink.request_id == request_id)
            .unwrap_or(false);
        if !is_owned {
            return;
        }

        if let Some(active_sink) = sink_guard.take() {
            active_sink.sink.stop();
        }
    }
}

/// The selected model id, falling back to the built-in Kokoro model.
pub fn selected_model_id(app_handle: &AppHandle) -> String {
    let selected = get_settings(app_handle).selected_model;
    if selected.is_empty() {
        MODEL_ID.to_string()
    } else {
        selected
    }
}

fn resolve_kokoro_model_dir(app_handle: &AppHandle) -> std::result::Result<PathBuf, String> {
    let app_data_candidate = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {}", e))?
        .join("models")
        .join("kokoro");
    if app_data_candidate.is_dir() {
        return Ok(app_data_candidate);
    }

    let resource_candidate = app_handle
        .path()
        .resolve("models/kokoro", BaseDirectory::Resource)
        .map_err(|e| format!("Failed to resolve app resource directory: {}", e))?;
    if resource_candidate.is_dir() {
        return Ok(resource_candidate);
    }

    Err(
        "Kokoro model directory not found. Expected <AppData>/models/kokoro or bundled resources/models/kokoro."
            .to_string(),
    )
}

fn open_output_stream(app_handle: &AppHandle) -> std::result::Result<MixerDeviceSink, String> {
    let settings = get_settings(app_handle);
    let selected_name = settings
        .selected_output_device
        .clone()
        .unwrap_or_else(|| "default".to_string());

    if selected_name != "default" {
        let host = cpal::default_host();
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                let Ok(desc) = device.description() else {
                    continue;
                };
                if desc.name() == selected_name {
                    return DeviceSinkBuilder::from_device(device)
                        .and_then(|builder| builder.open_stream())
                        .map_err(|e| format!("Failed to open selected output device: {}", e));
                }
            }
            error!(
                "Selected output device '{}' not found. Falling back to default.",
                selected_name
            );
        }
    }

    DeviceSinkBuilder::from_default_device()
        .and_then(|builder| builder.open_stream())
        .map_err(|e| format!("Failed to open default audio output: {}", e))
}

/// Soft target for the first chunk, kept small so chunk-1 synthesis finishes
/// quickly and playback starts almost immediately.
const FIRST_CHUNK_TARGET_CHARS: usize = 60;
const CHUNK_TARGET_CHARS: usize = 260;
const CHUNK_HARD_LIMIT_CHARS: usize = 320;
const SENTENCE_JOIN_SOFT_LIMIT_CHARS: usize = 380;

/// Chunk sizes ramp up: each chunk is at most as long as all the text queued
/// before it, so its synthesis overlaps playback of the earlier chunks instead
/// of starving the player. Jumping straight from a short first chunk to a
/// full-size one causes audible stalls on near-realtime CPUs.
/// Safety margin for buffered mode: start only when the buffered audio
/// covers the estimated remaining render time plus this share.
const BUFFER_SAFETY_FACTOR: f32 = 1.15;

/// Decides whether playback may start after a chunk was rendered.
///
/// - Stream: after the first chunk with audio.
/// - Complete: after the last chunk.
/// - Buffered: when playback can no longer catch up with rendering.
///   With real-time factor r (render seconds per audio second, measured so
///   far) and A seconds of audio still to render, playback empties the
///   buffer B while rendering refills it at 1/r. The gap-free condition is
///   B >= A * (r - 1) for a slow engine (r > 1), and in every case B must
///   cover rendering the next chunk. Both get a 15 % safety margin.
fn playback_start_reached(
    mode: PlaybackMode,
    last_chunk: bool,
    buffered_seconds: f32,
    synth_seconds: f32,
    chars_done: usize,
    total_chars: usize,
    next_chunk_chars: usize,
) -> bool {
    if buffered_seconds <= 0.0 {
        return false;
    }
    match mode {
        PlaybackMode::Stream => true,
        PlaybackMode::Complete => last_chunk,
        PlaybackMode::Buffered => {
            if last_chunk {
                return true;
            }
            let chars_done = chars_done.max(1) as f32;
            let rtf = synth_seconds / buffered_seconds;
            let audio_per_char = buffered_seconds / chars_done;
            let remaining_audio = audio_per_char * total_chars.saturating_sub(chars_done as usize) as f32;
            let catch_up = remaining_audio * (rtf - 1.0).max(0.0);
            let next_render = synth_seconds / chars_done * next_chunk_chars as f32;
            buffered_seconds >= catch_up.max(next_render) * BUFFER_SAFETY_FACTOR
        }
    }
}

fn chunk_target_chars(chars_already_chunked: usize, shorten_first_chunk: bool) -> usize {
    if !shorten_first_chunk {
        return CHUNK_TARGET_CHARS;
    }
    chars_already_chunked.clamp(FIRST_CHUNK_TARGET_CHARS, CHUNK_TARGET_CHARS)
}

fn chunked_chars(chunks: &[String]) -> usize {
    chunks.iter().map(String::len).sum()
}

/// Pushes `text` as one or more chunks. While the ramp is active, text longer
/// than the current target is split at a natural clause boundary (never
/// mid-word) so early chunks stay small enough to keep playback fed.
fn push_ramped_chunk(chunks: &mut Vec<String>, text: &str, shorten_first_chunk: bool) {
    let mut rest = text.trim().to_string();
    loop {
        let target = chunk_target_chars(chunked_chars(chunks), shorten_first_chunk);
        if target >= CHUNK_TARGET_CHARS || rest.len() <= target {
            break;
        }
        match split_at_clause_boundary(&rest, target) {
            (head, Some(tail)) => {
                chunks.push(head);
                rest = tail;
            }
            (_, None) => break,
        }
    }
    if !rest.is_empty() {
        chunks.push(rest);
    }
}

fn split_text_for_playback(text: &str, shorten_first_chunk: bool) -> Vec<String> {
    let normalized = normalize_text_whitespace(text);
    if normalized.is_empty() {
        return vec![];
    }

    let sentences = split_into_sentences(&normalized);
    let mut chunks: Vec<String> = Vec::new();
    let mut current_chunk = String::new();

    for sentence in sentences {
        let sentence_parts =
            split_long_segment(sentence, CHUNK_TARGET_CHARS, CHUNK_HARD_LIMIT_CHARS);

        for part in sentence_parts {
            if current_chunk.is_empty() {
                current_chunk.push_str(part);
                continue;
            }

            let current_target = chunk_target_chars(chunked_chars(&chunks), shorten_first_chunk);

            let candidate_len = current_chunk.len() + 1 + part.len();
            if candidate_len <= current_target
                || (candidate_len <= SENTENCE_JOIN_SOFT_LIMIT_CHARS
                    && current_chunk.len() < current_target / 2)
            {
                current_chunk.push(' ');
                current_chunk.push_str(part);
                continue;
            }

            push_ramped_chunk(&mut chunks, &current_chunk, shorten_first_chunk);
            current_chunk.clear();
            current_chunk.push_str(part);
        }
    }

    push_ramped_chunk(&mut chunks, &current_chunk, shorten_first_chunk);

    if chunks.is_empty() {
        vec![normalized]
    } else {
        chunks
    }
}

/// Split text at the best natural boundary near `target` chars.
///
/// Scans the window `[target/2 .. target*2]` for sentence-ending punctuation
/// (`.!?`), clause boundaries (`,;:`), and closing brackets/parens (`)]`).
/// Among all candidates the one closest to `target` wins — this keeps the
/// first chunk near the ideal size without favouring an awkward split just
/// because it is a comma rather than a closing paren.
///
/// If no natural boundary is found, returns the whole text unsplit — a longer
/// first chunk is preferable to a jarring mid-word break.
fn split_at_clause_boundary(text: &str, target: usize) -> (String, Option<String>) {
    let search_limit = (target * 2).min(text.len());
    let min_pos = target / 2;

    // (position, effective_distance) — lower distance wins.
    // Sentence-end and closing-bracket boundaries get a bonus (reduced
    // effective distance) so that a slightly farther `.` or `)` is preferred
    // over a nearby `,` when both are close to `target`.  This avoids
    // splitting inside parentheticals or between "August 19," and "1946)".
    const STRONG_BOUNDARY_BONUS: usize = 10;
    let mut best: Option<(usize, usize)> = None; // (pos, effective_distance)

    for (idx, ch) in text.char_indices() {
        let pos = idx + ch.len_utf8();
        if pos < min_pos {
            continue;
        }
        if idx > search_limit {
            break;
        }

        let ch_len = ch.len_utf8();
        if is_punctuation_between_ascii_digits(text, idx, ch_len) {
            continue;
        }

        let is_strong = is_sentence_boundary_char(ch) || matches!(ch, ')' | ']');
        let is_weak = is_clause_boundary_char(ch);

        if is_strong || is_weak {
            let raw_dist = pos.abs_diff(target);
            let eff_dist = if is_strong {
                raw_dist.saturating_sub(STRONG_BOUNDARY_BONUS)
            } else {
                raw_dist
            };
            if best.is_none() || eff_dist < best.unwrap().1 {
                best = Some((pos, eff_dist));
            }
        }
    }

    match best {
        Some((pos, _)) if pos < text.len() => {
            let head = text[..pos].trim().to_string();
            let tail = text[pos..].trim().to_string();
            if tail.is_empty() {
                (head, None)
            } else {
                (head, Some(tail))
            }
        }
        _ => {
            // No good boundary found — keep the text whole rather than
            // introducing a mid-word break.
            (text.trim().to_string(), None)
        }
    }
}

fn normalize_text_whitespace(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut in_whitespace = false;

    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_whitespace {
                normalized.push(' ');
                in_whitespace = true;
            }
        } else {
            normalized.push(ch);
            in_whitespace = false;
        }
    }

    normalized.trim().to_string()
}

fn split_into_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();

    for (idx, ch) in text.char_indices() {
        if !is_sentence_boundary_char(ch) {
            continue;
        }

        let ch_len = ch.len_utf8();
        if is_punctuation_between_ascii_digits(text, idx, ch_len) {
            continue;
        }

        let next_idx = idx + ch_len;
        let next_is_boundary = next_idx >= bytes.len()
            || bytes[next_idx].is_ascii_whitespace()
            || matches!(bytes[next_idx], b'"' | b'\'' | b')' | b']');
        if !next_is_boundary {
            continue;
        }

        let segment = text[start..next_idx].trim();
        if !segment.is_empty() {
            sentences.push(segment);
        }
        start = next_idx;
    }

    if start < text.len() {
        let trailing = text[start..].trim();
        if !trailing.is_empty() {
            sentences.push(trailing);
        }
    }

    if sentences.is_empty() {
        vec![text]
    } else {
        sentences
    }
}

fn split_long_segment(segment: &str, target: usize, hard_limit: usize) -> Vec<&str> {
    if segment.len() <= hard_limit {
        return vec![segment];
    }

    let mut parts = Vec::new();
    let mut offset = 0usize;
    while offset < segment.len() {
        let remaining = &segment[offset..];
        if remaining.len() <= hard_limit {
            parts.push(remaining.trim());
            break;
        }

        let boundary = find_split_index(remaining, target, hard_limit);
        let piece = remaining[..boundary].trim();
        if !piece.is_empty() {
            parts.push(piece);
        }
        offset += boundary;
    }

    parts
}

fn find_split_index(text: &str, target: usize, hard_limit: usize) -> usize {
    let mut last_sentence = None;
    let mut last_clause = None;
    let mut last_space = None;

    for (idx, ch) in text.char_indices() {
        if idx > hard_limit {
            break;
        }
        let ch_len = ch.len_utf8();
        let is_numeric_punctuation = is_punctuation_between_ascii_digits(text, idx, ch_len);
        if ch.is_whitespace() {
            last_space = Some(idx);
        }
        if is_sentence_boundary_char(ch) {
            if !is_numeric_punctuation {
                last_sentence = Some(idx + ch_len);
            }
        } else if is_clause_boundary_char(ch) && !is_numeric_punctuation {
            last_clause = Some(idx + ch_len);
        }
    }

    if let Some(idx) = last_sentence.filter(|idx| *idx >= target / 2 && *idx <= hard_limit) {
        return idx;
    }
    if let Some(idx) = last_clause.filter(|idx| *idx >= target / 2 && *idx <= hard_limit) {
        return idx;
    }
    if let Some(idx) = last_space.filter(|idx| *idx >= target / 2 && *idx <= hard_limit) {
        return idx;
    }

    let fallback = text
        .char_indices()
        .find(|(idx, _)| *idx >= hard_limit)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());

    adjust_split_index_for_numeric_connector(text, fallback)
}

fn is_sentence_boundary_char(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?' | '…')
}

fn is_clause_boundary_char(ch: char) -> bool {
    matches!(ch, ',' | ';' | ':')
}

fn is_punctuation_between_ascii_digits(text: &str, idx: usize, ch_len: usize) -> bool {
    let ch = text[idx..idx + ch_len].chars().next();
    if !matches!(ch, Some('.' | ',')) {
        return false;
    }

    let prev = text[..idx].chars().next_back();
    let next = text[idx + ch_len..].chars().next();

    matches!(
        (prev, next),
        (Some(left), Some(right)) if left.is_ascii_digit() && right.is_ascii_digit()
    )
}

fn adjust_split_index_for_numeric_connector(text: &str, idx: usize) -> usize {
    if idx == 0 || idx >= text.len() || !split_breaks_numeric_connector(text, idx) {
        return idx;
    }

    let mut forward = idx;
    while forward < text.len() && split_breaks_numeric_connector(text, forward) {
        let Some(next_ch) = text[forward..].chars().next() else {
            break;
        };
        forward += next_ch.len_utf8();
    }
    if !split_breaks_numeric_connector(text, forward) {
        return forward;
    }

    let mut backward = idx;
    while backward > 0 && split_breaks_numeric_connector(text, backward) {
        let Some((prev_idx, _)) = text[..backward].char_indices().next_back() else {
            break;
        };
        backward = prev_idx;
    }
    backward
}

fn split_breaks_numeric_connector(text: &str, idx: usize) -> bool {
    if idx == 0 || idx >= text.len() {
        return false;
    }

    let prev = text[..idx].chars().next_back();
    let next = text[idx..].chars().next();

    // Split between "2" and ".0"
    let next_breaks = matches!(next, Some('.' | ','))
        && prev.map(|c| c.is_ascii_digit()).unwrap_or(false)
        && text[idx + next.unwrap().len_utf8()..]
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);

    if next_breaks {
        return true;
    }

    // Split between "2." and "0"
    matches!(prev, Some('.' | ','))
        && next.map(|c| c.is_ascii_digit()).unwrap_or(false)
        && text[..idx - prev.unwrap().len_utf8()]
            .chars()
            .next_back()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        chunk_target_chars, estimate_kokoro_style_index, find_split_index,
        infer_kokoro_thread_count_for_cpu_count, normalize_kokoro_language_code, request_is_active,
        select_kokoro_voice_for_text, select_voice_for_language, split_into_sentences,
        split_text_for_playback, take_engine_for_active_request, CHUNK_TARGET_CHARS,
        FIRST_CHUNK_TARGET_CHARS, SENTENCE_JOIN_SOFT_LIMIT_CHARS,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::playback_start_reached;
    use crate::settings::PlaybackMode;

    #[test]
    fn playback_start_per_mode() {
        // Stream: first chunk with audio starts playback.
        assert!(playback_start_reached(PlaybackMode::Stream, false, 3.0, 4.0, 60, 600, 200));
        // Complete: only after the last chunk.
        assert!(!playback_start_reached(PlaybackMode::Complete, false, 30.0, 1.0, 590, 600, 10));
        assert!(playback_start_reached(PlaybackMode::Complete, true, 30.0, 40.0, 600, 600, 0));
        // Nothing buffered yet: never start.
        assert!(!playback_start_reached(PlaybackMode::Buffered, true, 0.0, 1.0, 600, 600, 0));
    }

    #[test]
    fn buffered_mode_waits_until_rendering_cannot_fall_behind() {
        // Slow engine, rtf 1.3 (5 s audio rendered in 6.5 s from 100 chars).
        // 1000 chars total: ~45 s audio still missing, catch-up 45*0.3 =
        // 13.5 s (x1.15) > 5 s buffered -> wait.
        assert!(!playback_start_reached(PlaybackMode::Buffered, false, 5.0, 6.5, 100, 1000, 260));
        // Same engine after 700 chars (35 s buffered): 15 s missing, catch-up
        // 4.5 s, next chunk 260 chars renders in ~16.9 s (x1.15 = 19.4 s):
        // 35 s buffer covers both -> start.
        assert!(playback_start_reached(PlaybackMode::Buffered, false, 35.0, 45.5, 700, 1000, 260));
        // Fast engine (rtf 0.3, 5 s in 1.5 s): the next 260-char chunk
        // renders in 3.9 s (x1.15 = 4.5 s) < 5 s buffered -> start at once.
        assert!(playback_start_reached(PlaybackMode::Buffered, false, 5.0, 1.5, 100, 1000, 260));
        // Fast engine but a huge next chunk it cannot render in time: wait.
        assert!(!playback_start_reached(PlaybackMode::Buffered, false, 1.0, 0.3, 20, 1000, 320));
    }
    use std::time::Duration;

    #[test]
    fn keeps_sentence_boundaries_for_short_text() {
        let chunks = split_text_for_playback("First sentence. Second sentence?", true);
        assert_eq!(chunks, vec!["First sentence. Second sentence?"]);
    }

    #[test]
    fn splits_long_text_without_empty_chunks() {
        let text = "This is a long sentence with several clauses, and it keeps going so we can force a split point near punctuation. \
            Here is another sentence with enough additional words to exceed the hard limit while still keeping punctuation intact.";
        let chunks = split_text_for_playback(&text.repeat(8), true);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|chunk| !chunk.trim().is_empty()));
    }

    #[test]
    fn keeps_decimal_numbers_in_sentence_splitting() {
        let sentences = split_into_sentences("Version 2.0 is out. Install now.");
        assert_eq!(sentences, vec!["Version 2.0 is out.", "Install now."]);
    }

    #[test]
    fn does_not_use_decimal_or_comma_between_digits_as_chunk_boundary() {
        let decimal_text = format!("{}2.0 {}", "word ".repeat(40), "word ".repeat(40));
        let decimal_chunks = split_text_for_playback(&decimal_text, true);
        for pair in decimal_chunks.windows(2) {
            assert!(!(pair[0].trim_end().ends_with("2.") && pair[1].trim_start().starts_with('0')));
        }

        let comma_text = format!("{}1,000 {}", "word ".repeat(40), "word ".repeat(40));
        let comma_chunks = split_text_for_playback(&comma_text, true);
        for pair in comma_chunks.windows(2) {
            assert!(
                !(pair[0].trim_end().ends_with("1,") && pair[1].trim_start().starts_with("000"))
            );
        }
    }

    #[test]
    fn fallback_split_does_not_cut_inside_decimal_token() {
        let text = "prefixv2.0suffix";
        let dot_idx = text.find('.').expect("decimal point should exist");
        let split_idx = find_split_index(text, dot_idx, dot_idx);
        assert_eq!(split_idx, dot_idx + 2);
    }

    #[test]
    fn split_index_skips_numeric_decimal_boundary() {
        let text = "alpha beta gamma 2.0 delta epsilon";
        let decimal_boundary = text.find('.').expect("decimal point should exist") + 1;
        let split_idx = find_split_index(text, 12, decimal_boundary);
        assert_ne!(split_idx, decimal_boundary);
    }

    #[test]
    fn first_chunk_splits_at_clause_boundary_not_mid_word() {
        // First sentence is 171 chars — well over FIRST_CHUNK_TARGET_CHARS (60).
        // The chunker should split at the closing paren (a strong boundary)
        // rather than at a mid-sentence word boundary.
        let text = "William Jefferson Clinton (né Blythe III; born August 19, 1946) \
            is an American politician and lawyer who served as the 42nd president \
            of the United States from 1993 to 2001. A member of the Democratic \
            Party, he previously served as the attorney general of Arkansas.";
        let chunks = split_text_for_playback(text, true);
        assert!(chunks.len() >= 2, "should split into multiple chunks");
        // First chunk must end at the closing paren — a natural clause boundary.
        assert!(
            chunks[0].ends_with(')') || chunks[0].ends_with('.'),
            "first chunk should end at a natural boundary, got: {:?}",
            chunks[0]
        );
        // First chunk should be well under 100 chars for fast synthesis.
        assert!(
            chunks[0].len() <= 100,
            "first chunk should be ≤100 chars for fast synthesis, got {} chars",
            chunks[0].len()
        );
    }

    fn words(text: &str) -> Vec<&str> {
        text.split_whitespace().collect()
    }

    #[test]
    fn chunking_preserves_every_word_and_punctuation_mark_in_order() {
        let text = "Hello, world! This is Parrot; it reads text aloud. Does it pause? \
                    Yes: at commas, periods, and colons. "
            .repeat(12);
        for shorten_first_chunk in [true, false] {
            let chunks = split_text_for_playback(&text, shorten_first_chunk);
            assert!(chunks.len() > 1);
            assert_eq!(words(&chunks.join(" ")), words(&text));
        }
    }

    #[test]
    fn chunks_never_exceed_the_hard_limit() {
        let long_sentence = "word ".repeat(400);
        let text = format!("{long_sentence}. Short tail sentence.");
        for shorten_first_chunk in [true, false] {
            for chunk in split_text_for_playback(&text, shorten_first_chunk) {
                assert!(!chunk.trim().is_empty());
                assert!(
                    chunk.len() <= SENTENCE_JOIN_SOFT_LIMIT_CHARS,
                    "chunk too long: {} chars",
                    chunk.len()
                );
            }
        }
    }

    #[test]
    fn fast_first_response_keeps_first_chunk_short() {
        let text =
            "The quick brown fox jumps over the lazy dog, then runs into the forest. ".repeat(10);
        let fast = split_text_for_playback(&text, true);
        let normal = split_text_for_playback(&text, false);
        assert!(fast[0].len() <= FIRST_CHUNK_TARGET_CHARS + 20);
        assert!(normal[0].len() > fast[0].len());
    }

    #[test]
    fn empty_or_whitespace_text_yields_no_chunks() {
        assert!(split_text_for_playback("", true).is_empty());
        assert!(split_text_for_playback("   \n\t ", false).is_empty());
    }

    #[test]
    fn early_chunks_ramp_up_to_full_size_when_fast_first_response_enabled() {
        let paragraph = "This sentence has about sixty characters in it, roughly. ";
        let text = paragraph.repeat(20);
        let chunks = split_text_for_playback(&text, true);
        assert!(chunks.len() > 3);
        assert!(chunks[0].len() <= FIRST_CHUNK_TARGET_CHARS);
        let mut queued = 0;
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(chunk.len() <= queued.max(FIRST_CHUNK_TARGET_CHARS) + paragraph.len());
            queued += chunk.len();
        }
        assert!(chunks
            .iter()
            .any(|chunk| chunk.len() > CHUNK_TARGET_CHARS / 2));
        assert_eq!(words(&chunks.join(" ")), words(&text));
    }

    #[test]
    fn chunk_targets_ramp_up_only_with_fast_first_response() {
        assert_eq!(chunk_target_chars(0, true), FIRST_CHUNK_TARGET_CHARS);
        assert_eq!(chunk_target_chars(100, true), 100);
        assert_eq!(chunk_target_chars(10_000, true), CHUNK_TARGET_CHARS);
        assert_eq!(chunk_target_chars(0, false), CHUNK_TARGET_CHARS);
        assert_eq!(chunk_target_chars(100, false), CHUNK_TARGET_CHARS);
    }

    /// Models the single-engine pipeline: chunks are synthesized one after
    /// another while earlier ones play, audio length is proportional to chunk
    /// length, and synthesis takes `rtf` times the audio length. Returns total
    /// seconds the player waits for a chunk after playback has started.
    fn simulated_playback_stall_secs(chunks: &[String], rtf: f32) -> f32 {
        const AUDIO_SECS_PER_CHAR: f32 = 0.072;
        let mut synth_done = 0.0_f32;
        let ready_at: Vec<f32> = chunks
            .iter()
            .map(|chunk| {
                synth_done += rtf * chunk.len() as f32 * AUDIO_SECS_PER_CHAR;
                synth_done
            })
            .collect();
        let mut playhead = ready_at[0];
        let mut stall = 0.0;
        for (chunk, ready) in chunks.iter().zip(ready_at) {
            if ready > playhead {
                stall += ready - playhead;
                playhead = ready;
            }
            playhead += chunk.len() as f32 * AUDIO_SECS_PER_CHAR;
        }
        stall
    }

    // Text and timings from the issue #1 / #7 repro: Kokoro ran at rtf ~0.8-0.9,
    // and the 180-char third chunk left a ~3s silence.
    const MULTI_PARAGRAPH_TEXT: &str = "The first paragraph is short, but it has commas, semicolons; and a question? It also ends with a period.

The second paragraph keeps going, with more clauses, more commas, and more pauses, so that the text is split into many segments before phonemization.

Finally, the third paragraph closes the test. If you can hear this sentence, playback reached the end!";

    #[test]
    fn multi_paragraph_playback_does_not_stall_at_near_realtime_synthesis() {
        let chunks = split_text_for_playback(MULTI_PARAGRAPH_TEXT, true);
        assert_eq!(words(&chunks.join(" ")), words(MULTI_PARAGRAPH_TEXT));
        for rtf in [0.3, 0.4, 0.5] {
            let stall = simulated_playback_stall_secs(&chunks, rtf);
            assert!(
                stall < 0.25,
                "rtf={rtf}: player stalled {stall:.2}s with chunks {chunks:?}"
            );
        }
    }

    #[test]
    fn long_text_playback_does_not_stall_at_near_realtime_synthesis() {
        let text = MULTI_PARAGRAPH_TEXT.repeat(6);
        let chunks = split_text_for_playback(&text, true);
        for rtf in [0.3, 0.4, 0.5] {
            let stall = simulated_playback_stall_secs(&chunks, rtf);
            assert!(stall < 0.25, "rtf={rtf}: player stalled {stall:.2}s");
        }
    }

    #[test]
    fn kokoro_tuning_uses_all_cores_on_constrained_cpu() {
        // 2 CPUs → no cores reserved → all 2 threads available
        assert_eq!(infer_kokoro_thread_count_for_cpu_count(2), 2);
    }

    #[test]
    fn kokoro_tuning_reserves_one_core_on_larger_cpu() {
        // 8 CPUs → 1 core reserved for UI/audio → 7 threads available
        assert_eq!(infer_kokoro_thread_count_for_cpu_count(8), 7);
    }

    #[test]
    fn style_index_estimate_is_non_zero() {
        let chunks = vec!["Hello world.".to_string(), "How are you?".to_string()];
        assert!(estimate_kokoro_style_index(&chunks) > 0);
    }

    #[test]
    fn selects_french_voice_when_available() {
        let voices = vec![
            "af_heart".to_string(),
            "ff_siwis".to_string(),
            "bf_emma".to_string(),
        ];
        let selected = select_kokoro_voice_for_text(Some("fr"), None, &voices);
        assert_eq!(selected, "ff_siwis");
    }

    #[test]
    fn normalizes_all_kokoro_supported_languages() {
        assert_eq!(normalize_kokoro_language_code(Some("en-US")), Some("en-us"));
        assert_eq!(normalize_kokoro_language_code(Some("en-GB")), Some("en-gb"));
        assert_eq!(normalize_kokoro_language_code(Some("es-AR")), Some("es"));
        assert_eq!(normalize_kokoro_language_code(Some("fr-CA")), Some("fr"));
        assert_eq!(normalize_kokoro_language_code(Some("hi-IN")), Some("hi"));
        assert_eq!(normalize_kokoro_language_code(Some("it-IT")), Some("it"));
        assert_eq!(normalize_kokoro_language_code(Some("ja-JP")), Some("ja"));
        assert_eq!(normalize_kokoro_language_code(Some("pt-PT")), Some("pt-br"));
        assert_eq!(normalize_kokoro_language_code(Some("zh-Hant")), Some("cmn"));
        assert_eq!(normalize_kokoro_language_code(Some("zh-CN")), Some("cmn"));
        assert_eq!(normalize_kokoro_language_code(Some("yue")), Some("cmn"));
        assert_eq!(normalize_kokoro_language_code(Some("cmn")), Some("cmn"));
    }

    #[test]
    fn selects_voice_for_each_supported_kokoro_language() {
        let voices = vec![
            "af_heart".to_string(),
            "bf_emma".to_string(),
            "ef_dora".to_string(),
            "ff_siwis".to_string(),
            "hf_beta".to_string(),
            "if_alpha".to_string(),
            "jf_alpha".to_string(),
            "pf_dora".to_string(),
            "zf_xiaobei".to_string(),
        ];

        assert!(select_voice_for_language("en-us", &voices).starts_with("af_"));
        assert!(select_voice_for_language("en-gb", &voices).starts_with("bf_"));
        assert!(select_voice_for_language("es", &voices).starts_with("ef_"));
        assert!(select_voice_for_language("fr", &voices).starts_with("ff_"));
        assert!(select_voice_for_language("hi", &voices).starts_with("hf_"));
        assert!(select_voice_for_language("it", &voices).starts_with("if_"));
        assert!(select_voice_for_language("ja", &voices).starts_with("jf_"));
        assert!(select_voice_for_language("pt-br", &voices).starts_with("pf_"));
        assert!(select_voice_for_language("cmn", &voices).starts_with("zf_"));
    }

    #[test]
    fn selected_voice_override_has_priority() {
        let voices = vec![
            "af_heart".to_string(),
            "ff_siwis".to_string(),
            "jf_alpha".to_string(),
        ];

        let selected = select_kokoro_voice_for_text(Some("fr"), Some("jf_alpha"), &voices);
        assert_eq!(selected, "jf_alpha");
    }

    #[test]
    fn selected_voice_override_falls_back_when_unavailable() {
        let voices = vec!["af_heart".to_string(), "ff_siwis".to_string()];
        let selected = select_kokoro_voice_for_text(Some("fr"), Some("jf_alpha"), &voices);
        assert_eq!(selected, "ff_siwis");
    }

    #[test]
    fn request_active_check_requires_matching_generation_and_request() {
        let generation = Arc::new(AtomicU64::new(11));
        let active_request = Arc::new(AtomicU64::new(11));
        assert!(request_is_active(&generation, &active_request, 11));

        generation.store(12, Ordering::SeqCst);
        assert!(!request_is_active(&generation, &active_request, 11));

        generation.store(11, Ordering::SeqCst);
        active_request.store(0, Ordering::SeqCst);
        assert!(!request_is_active(&generation, &active_request, 11));
    }

    #[test]
    fn engine_take_aborts_when_request_is_cancelled() {
        let engine_slot = Arc::new(Mutex::new(None::<super::TtsEngine>));
        let generation = Arc::new(AtomicU64::new(5));
        let active_request = Arc::new(AtomicU64::new(5));

        // Slot is None, so take_engine will keep polling.
        // Cancel the request so it gives up.
        let engine_slot_for_worker = Arc::clone(&engine_slot);
        let generation_for_worker = Arc::clone(&generation);
        let active_request_for_worker = Arc::clone(&active_request);
        let worker = thread::spawn(move || {
            take_engine_for_active_request(
                &engine_slot_for_worker,
                &generation_for_worker,
                &active_request_for_worker,
                5,
            )
            .is_none()
        });

        thread::sleep(Duration::from_millis(10));
        generation.store(6, Ordering::SeqCst);
        active_request.store(0, Ordering::SeqCst);

        assert!(
            worker.join().expect("worker thread should join"),
            "take_engine should abort when request is cancelled"
        );
    }
}
