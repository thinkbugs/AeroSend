import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useAppStore } from "../stores/appStore";
import type { EncoderChoice, Resolution } from "../types";

interface EncoderStatus {
  ffmpeg: string;
  x264: boolean;
  nvenc: boolean;
  qsv: boolean;
}

const FPS_OPTIONS = [24, 30, 60];
const BITRATE_OPTIONS = [2_000_000, 4_000_000, 6_000_000, 8_000_000, 12_000_000];
const RESOLUTION_OPTIONS: { value: Resolution; label: string }[] = [
  { value: "native", label: "Native (match screen)" },
  { value: "1080p", label: "1080p" },
  { value: "720p", label: "720p" },
  { value: "480p", label: "480p" },
];

export default function Settings() {
  const {
    settings,
    settingsLoaded,
    settingsError,
    loadSettings,
    updateSettings,
  } = useAppStore();
  const [encoderStatus, setEncoderStatus] = useState<EncoderStatus | null>(null);
  const [encoderError, setEncoderError] = useState<string | null>(null);

  useEffect(() => {
    loadSettings();
    invoke<EncoderStatus>("get_encoder_status")
      .then(setEncoderStatus)
      .catch((e) => setEncoderError(String(e)));
  }, []);

  const encoders: { value: EncoderChoice; label: string; available: boolean }[] = [
    { value: "auto", label: "Auto (software, lowest latency)", available: true },
    { value: "x264", label: "x264 (software)", available: encoderStatus?.x264 ?? true },
    { value: "nvenc", label: "NVIDIA NVENC", available: encoderStatus?.nvenc ?? false },
    { value: "qsv", label: "Intel Quick Sync", available: encoderStatus?.qsv ?? false },
  ];

  return (
    <div className="settings-page">
      <h1>Settings</h1>
      <p className="section-desc">
        Applies to the next connection. Saved to disk and reused on the next start.
      </p>

      {(settingsError || encoderError) && (
        <div className="setting-warning">{settingsError ?? encoderError}</div>
      )}

      <div className="settings-section">
        <h2>Video</h2>
        <div className="setting-row">
          <label htmlFor="fps">Frame rate</label>
          <select
            id="fps"
            disabled={!settingsLoaded}
            value={settings.fps}
            onChange={(e) => updateSettings({ fps: Number(e.target.value) })}
          >
            {FPS_OPTIONS.map((fps) => (
              <option key={fps} value={fps}>
                {fps} fps
              </option>
            ))}
          </select>
        </div>
        <div className="setting-row">
          <label htmlFor="bitrate">Bitrate</label>
          <select
            id="bitrate"
            disabled={!settingsLoaded}
            value={settings.bitrate}
            onChange={(e) => updateSettings({ bitrate: Number(e.target.value) })}
          >
            {BITRATE_OPTIONS.map((bps) => (
              <option key={bps} value={bps}>
                {bps / 1_000_000} Mbps
              </option>
            ))}
          </select>
        </div>
        <div className="setting-row">
          <label htmlFor="resolution">Resolution</label>
          <select
            id="resolution"
            disabled={!settingsLoaded}
            value={settings.resolution}
            onChange={(e) => updateSettings({ resolution: e.target.value as Resolution })}
          >
            {RESOLUTION_OPTIONS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </div>
        <div className="setting-row">
          <label htmlFor="encoder">Encoder</label>
          <select
            id="encoder"
            disabled={!settingsLoaded}
            value={settings.encoder}
            onChange={(e) => updateSettings({ encoder: e.target.value as EncoderChoice })}
          >
            {encoders.map((option) => (
              <option
                key={option.value}
                value={option.value}
                disabled={!!encoderStatus && !option.available}
              >
                {option.available || !encoderStatus ? option.label : `${option.label} — unavailable`}
              </option>
            ))}
          </select>
        </div>
        {encoderStatus && (
          <p className="setting-hint">
            ffmpeg: {encoderStatus.ffmpeg} · x264 {encoderStatus.x264 ? "ready" : "missing"} · NVENC{" "}
            {encoderStatus.nvenc ? "ready" : "missing"} · QSV{" "}
            {encoderStatus.qsv ? "ready" : "missing"}
          </p>
        )}
      </div>

      <div className="settings-section">
        <h2>Audio</h2>
        <div className="setting-row">
          <label htmlFor="audio">Cast system audio (WASAPI loopback, 128 kbps AAC)</label>
          <label className="toggle">
            <input
              id="audio"
              type="checkbox"
              disabled={!settingsLoaded}
              checked={settings.audio}
              onChange={(e) => updateSettings({ audio: e.target.checked })}
            />
            <span className="toggle-slider" />
          </label>
        </div>
      </div>

      <div className="settings-section">
        <h2>Annotation</h2>
        <div className="setting-row">
          <label htmlFor="annotation">Draw on the mirrored image (planned for a later build)</label>
          <label className="toggle">
            <input
              id="annotation"
              type="checkbox"
              disabled={!settingsLoaded}
              checked={settings.annotation}
              onChange={(e) => updateSettings({ annotation: e.target.checked })}
            />
            <span className="toggle-slider" />
          </label>
        </div>
      </div>

      <div className="settings-section">
        <h2>About</h2>
        <div className="about-info">
          <p>AeroSend v0.1.0</p>
          <p>Professional AirPlay Sender for Windows</p>
          <p className="muted">Built with Tauri + React + Rust</p>
        </div>
      </div>
    </div>
  );
}
