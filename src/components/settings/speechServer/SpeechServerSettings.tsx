import React, { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import {
  commands,
  type ServerConfigView,
  type ServerModelView,
  type ServerParamsView,
  type ServerStatus,
} from "@/bindings";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { SettingContainer } from "../../ui/SettingContainer";
import { Dropdown } from "../../ui/Dropdown";
import { Slider } from "../../ui/Slider";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { Input } from "../../ui/Input";
import { Textarea } from "../../ui/Textarea";
import { Button } from "../../ui/Button";

const CLONE_PREFIX = "clone:";
const AUDIO_EXTENSIONS = ["wav", "mp3", "m4a", "aiff", "aif", "flac", "caf", "aac"];

const LANGUAGE_LABELS: Record<string, string> = {
  de: "Deutsch",
  en: "English",
  es: "Español",
  fr: "Français",
  zh: "中文",
  ko: "한국어",
  ja: "日本語",
};

/** One generation knob as shown in the settings window. */
interface KnobSpec {
  key: keyof ServerParamsView;
  min: number;
  max: number;
  step: number;
  /** The model's own default, used when the knob is unset. */
  fallback: number;
  integer?: boolean;
}

/** Knobs whose effect was measured, per engine family. */
const KNOBS: Record<string, KnobSpec[]> = {
  ming: [
    { key: "cfg_scale", min: 1, max: 5, step: 0.1, fallback: 2.0 },
    { key: "ddpm_steps", min: 4, max: 30, step: 1, fallback: 10, integer: true },
    { key: "sigma", min: 0.05, max: 1, step: 0.05, fallback: 0.25 },
  ],
  chatterbox: [
    { key: "exaggeration", min: 0, max: 1, step: 0.05, fallback: 0.1 },
    { key: "cfg_weight", min: 0, max: 1, step: 0.05, fallback: 0.5 },
    { key: "min_p", min: 0, max: 0.3, step: 0.01, fallback: 0.05 },
    { key: "repetition_penalty", min: 1, max: 2, step: 0.05, fallback: 1.2 },
  ],
};

const EMPTY_PARAMS: ServerParamsView = {
  top_p: null,
  top_k: null,
  repetition_penalty: null,
  cfg_scale: null,
  ddpm_steps: null,
  sigma: null,
  exaggeration: null,
  cfg_weight: null,
  min_p: null,
};

const fileNameStem = (path: string): string => {
  const base = path.split(/[\\/]/).pop() ?? path;
  return base.replace(/\.[^.]+$/, "");
};

export const SpeechServerSettings: React.FC = () => {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ServerStatus | null>(null);
  const [config, setConfig] = useState<ServerConfigView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [previewText, setPreviewText] = useState(
    "Guten Tag. So klingt die gewählte Stimme mit diesen Einstellungen.",
  );

  // New voice sample form.
  const [samplePath, setSamplePath] = useState<string | null>(null);
  const [sampleName, setSampleName] = useState("");
  const [sampleText, setSampleText] = useState("");
  const [transcribing, setTranscribing] = useState(false);

  const refresh = useCallback(async () => {
    const s = await commands.getServerStatus();
    if (s.status === "ok") setStatus(s.data);
    const c = await commands.getServerConfig();
    if (c.status === "ok") {
      setConfig(c.data);
      setError(null);
    } else {
      setConfig(null);
      setError(c.error);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  /** Runs a command, shows its error, then reloads the view. */
  const run = useCallback(
    async (action: () => Promise<{ status: string; error?: string }>) => {
      setBusy(true);
      try {
        const result = await action();
        if (result.status === "error") setError(result.error ?? "error");
        else setError(null);
      } finally {
        setBusy(false);
        await refresh();
      }
    },
    [refresh],
  );

  const activeModel: ServerModelView | undefined = useMemo(() => {
    if (!config) return undefined;
    return (
      config.models.find((m) => m.id === config.active_model) ??
      config.models[0]
    );
  }, [config]);

  const usableSamples = useMemo(
    () => (config?.samples ?? []).filter((s) => !s.missing),
    [config],
  );

  const voiceOptions = useMemo(() => {
    if (!activeModel) return [];
    const builtIn =
      activeModel.engine_kind === "chatterbox"
        ? activeModel.clones
            .filter((c) => !c.missing)
            .map((c) => ({ value: c.name, label: c.name }))
        : activeModel.voices.map((v) => ({ value: v, label: v }));
    const samples = usableSamples.map((s) => ({
      value: `${CLONE_PREFIX}${s.name}`,
      label: t("settings.speechServer.sampleVoiceLabel", { name: s.name }),
    }));
    return [...builtIn, ...samples];
  }, [activeModel, usableSamples, t]);

  const knobs = activeModel ? (KNOBS[activeModel.engine_kind] ?? []) : [];
  const params = activeModel?.params ?? EMPTY_PARAMS;

  const setKnob = (key: keyof ServerParamsView, value: number | null) => {
    if (!activeModel) return;
    run(() =>
      commands.setServerParams(activeModel.id, { ...params, [key]: value }),
    );
  };

  const pickSampleFile = async () => {
    const picked = await open({
      multiple: false,
      directory: false,
      filters: [
        { name: t("settings.speechServer.audioFiles"), extensions: AUDIO_EXTENSIONS },
      ],
    });
    if (typeof picked !== "string") return;
    setSamplePath(picked);
    if (!sampleName.trim()) setSampleName(fileNameStem(picked));
  };

  const transcribe = async () => {
    if (!samplePath) return;
    setTranscribing(true);
    try {
      const result = await commands.transcribeServerSample(
        samplePath,
        config?.lang_code ?? null,
      );
      if (result.status === "ok") {
        setSampleText(result.data);
        setError(null);
      } else {
        setError(result.error);
      }
    } finally {
      setTranscribing(false);
    }
  };

  const addSample = async () => {
    if (!samplePath || !sampleName.trim()) return;
    await run(async () => {
      const result = await commands.addServerSample(
        sampleName.trim(),
        samplePath,
        sampleText,
      );
      if (result.status === "ok") {
        setSamplePath(null);
        setSampleName("");
        setSampleText("");
      }
      return result;
    });
  };

  const offline = !status?.alive;

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      {error && (
        <div className="px-4 py-2 rounded-lg border border-red-500/40 bg-red-500/10 text-sm">
          {error}
        </div>
      )}

      <SettingsGroup title={t("settings.speechServer.statusTitle")}>
        <SettingContainer
          title={t("settings.speechServer.server")}
          description={t("settings.speechServer.serverDescription")}
          grouped
        >
          <div className="flex items-center gap-3">
            <span className={`text-sm ${offline ? "text-red-400" : "text-green-500"}`}>
              {offline
                ? t("settings.speechServer.offline")
                : t("settings.speechServer.online", {
                    host: status?.host,
                    port: status?.port,
                  })}
            </span>
            <Button variant="secondary" size="sm" onClick={() => run(commands.reloadServer)} disabled={busy}>
              {t("settings.speechServer.reload")}
            </Button>
          </div>
        </SettingContainer>
      </SettingsGroup>

      {config && activeModel && (
        <>
          <SettingsGroup title={t("settings.speechServer.voiceTitle")}>
            <SettingContainer
              title={t("settings.speechServer.model")}
              description={t("settings.speechServer.modelDescription")}
              grouped
            >
              <Dropdown
                options={config.models.map((m) => ({ value: m.id, label: m.alias }))}
                selectedValue={activeModel.id}
                onSelect={(id) => run(() => commands.setServerModel(id))}
                disabled={busy}
              />
            </SettingContainer>
            <SettingContainer
              title={t("settings.speechServer.voice")}
              description={t("settings.speechServer.voiceDescription")}
              grouped
            >
              <Dropdown
                options={voiceOptions}
                selectedValue={activeModel.default_voice}
                onSelect={(voice) => run(() => commands.setServerVoice(activeModel.id, voice))}
                disabled={busy || voiceOptions.length === 0}
                placeholder={t("settings.speechServer.noVoices")}
              />
            </SettingContainer>
            <SettingContainer
              title={t("settings.speechServer.language")}
              description={t("settings.speechServer.languageDescription")}
              grouped
            >
              <Dropdown
                options={activeModel.languages.map((code) => ({
                  value: code,
                  label: LANGUAGE_LABELS[code] ?? code,
                }))}
                selectedValue={config.lang_code}
                onSelect={(code) => run(() => commands.setServerLanguage(code))}
                disabled={busy}
              />
            </SettingContainer>
            <SettingContainer
              title={t("settings.speechServer.preview")}
              description={t("settings.speechServer.previewDescription")}
              grouped
              layout="stacked"
            >
              <div className="flex gap-2">
                <Input
                  className="flex-grow"
                  value={previewText}
                  onChange={(e) => setPreviewText(e.target.value)}
                />
                <Button
                  variant="primary-soft"
                  onClick={() => run(() => commands.previewServerVoice(previewText))}
                  disabled={busy || offline || !previewText.trim()}
                >
                  {t("settings.speechServer.play")}
                </Button>
              </div>
            </SettingContainer>
          </SettingsGroup>

          <SettingsGroup
            title={t("settings.speechServer.generationTitle")}
            description={t("settings.speechServer.generationDescription")}
          >
            <Slider
              label={t("settings.speechServer.temperature")}
              description={t("settings.speechServer.temperatureDescription")}
              value={config.temperature ?? activeModel.temperature}
              min={0.05}
              max={1.2}
              step={0.05}
              onChange={(v) => run(() => commands.setServerTemperature(v))}
              disabled={busy}
              grouped
            />
            <SettingContainer
              title={t("settings.speechServer.seed")}
              description={t("settings.speechServer.seedDescription")}
              grouped
            >
              <div className="flex items-center gap-2">
                <Input
                  variant="compact"
                  className="w-24"
                  type="number"
                  value={config.seed ?? ""}
                  placeholder={t("settings.speechServer.seedRandom")}
                  onChange={(e) => {
                    const raw = e.target.value.trim();
                    const seed = raw === "" ? null : Math.trunc(Number(raw));
                    if (seed === null || Number.isFinite(seed)) {
                      run(() => commands.setServerSeed(seed));
                    }
                  }}
                  disabled={busy}
                />
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() =>
                    run(() => commands.setServerSeed(Math.floor(Math.random() * 100000)))
                  }
                  disabled={busy}
                >
                  {t("settings.speechServer.seedNew")}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => run(() => commands.setServerSeed(null))}
                  disabled={busy || config.seed === null}
                >
                  {t("settings.speechServer.seedRandom")}
                </Button>
              </div>
            </SettingContainer>
            {activeModel.engine_kind !== "chatterbox" && (
              <ToggleSwitch
                label={t("settings.speechServer.lockVoice")}
                description={t("settings.speechServer.lockVoiceDescription")}
                checked={config.lock_voice}
                onChange={(on) => run(() => commands.setServerLockVoice(on))}
                isUpdating={busy}
                grouped
              />
            )}
            {knobs.map((knob) => {
              const current = params[knob.key];
              return (
                <Slider
                  key={knob.key}
                  label={t(`settings.speechServer.knobs.${knob.key}.label`)}
                  description={t(`settings.speechServer.knobs.${knob.key}.description`, {
                    fallback: knob.fallback,
                  })}
                  value={current ?? knob.fallback}
                  min={knob.min}
                  max={knob.max}
                  step={knob.step}
                  formatValue={(v) => (knob.integer ? v.toFixed(0) : v.toFixed(2))}
                  onChange={(v) => setKnob(knob.key, knob.integer ? Math.round(v) : v)}
                  disabled={busy}
                  grouped
                />
              );
            })}
            {knobs.length > 0 && (
              <SettingContainer
                title={t("settings.speechServer.resetKnobs")}
                description={t("settings.speechServer.resetKnobsDescription")}
                grouped
              >
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => run(() => commands.setServerParams(activeModel.id, EMPTY_PARAMS))}
                  disabled={busy}
                >
                  {t("settings.speechServer.reset")}
                </Button>
              </SettingContainer>
            )}
          </SettingsGroup>
        </>
      )}

      {config && (
        <SettingsGroup
          title={t("settings.speechServer.samplesTitle")}
          description={t("settings.speechServer.samplesDescription")}
        >
          {config.samples.length === 0 && (
            <div className="px-4 py-3 text-sm text-mid-gray">
              {t("settings.speechServer.noSamples")}
            </div>
          )}
          {config.samples.map((sample) => (
            <SettingContainer
              key={sample.name}
              title={sample.name}
              description={sample.ref_text || t("settings.speechServer.noTranscript")}
              descriptionMode="inline"
              grouped
            >
              <div className="flex items-center gap-2">
                {sample.trimmed_from !== null && sample.trimmed_to !== null && (
                  <span className="text-xs text-mid-gray">
                    {t("settings.speechServer.sampleTrimmed", {
                      from: sample.trimmed_from.toFixed(0),
                      to: sample.trimmed_to.toFixed(1),
                    })}
                  </span>
                )}
                {sample.missing && (
                  <span className="text-xs text-red-400">
                    {t("settings.speechServer.sampleMissing")}
                  </span>
                )}
                {activeModel && !sample.missing && (
                  <Button
                    variant="secondary"
                    size="sm"
                    onClick={() =>
                      run(() =>
                        commands.setServerVoice(activeModel.id, `${CLONE_PREFIX}${sample.name}`),
                      )
                    }
                    disabled={busy || activeModel.default_voice === `${CLONE_PREFIX}${sample.name}`}
                  >
                    {t("settings.speechServer.useSample")}
                  </Button>
                )}
                <Button
                  variant="danger-ghost"
                  size="sm"
                  onClick={() => run(() => commands.removeServerSample(sample.name))}
                  disabled={busy}
                >
                  {t("settings.speechServer.delete")}
                </Button>
              </div>
            </SettingContainer>
          ))}

          <SettingContainer
            title={t("settings.speechServer.addSample")}
            description={t("settings.speechServer.addSampleDescription")}
            grouped
            layout="stacked"
            descriptionMode="inline"
          >
            <div className="space-y-2">
              <div className="flex items-center gap-2">
                <Button variant="secondary" size="sm" onClick={pickSampleFile} disabled={busy}>
                  {t("settings.speechServer.chooseFile")}
                </Button>
                <span className="text-xs text-mid-gray truncate">
                  {samplePath ?? t("settings.speechServer.noFile")}
                </span>
              </div>
              <Input
                className="w-full"
                value={sampleName}
                placeholder={t("settings.speechServer.sampleName")}
                onChange={(e) => setSampleName(e.target.value)}
                disabled={busy}
              />
              <Textarea
                className="w-full"
                variant="compact"
                value={sampleText}
                placeholder={t("settings.speechServer.sampleTranscript")}
                onChange={(e) => setSampleText(e.target.value)}
                disabled={busy}
              />
              <div className="flex gap-2">
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={transcribe}
                  disabled={!samplePath || transcribing || offline}
                >
                  {transcribing
                    ? t("settings.speechServer.transcribing")
                    : t("settings.speechServer.transcribe")}
                </Button>
                <Button
                  variant="primary"
                  size="sm"
                  onClick={addSample}
                  disabled={!samplePath || !sampleName.trim() || busy || offline}
                >
                  {t("settings.speechServer.saveSample")}
                </Button>
              </div>
            </div>
          </SettingContainer>
        </SettingsGroup>
      )}
    </div>
  );
};
