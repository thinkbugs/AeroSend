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
  { value: "native", label: "原生（跟随屏幕）" },
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
    { value: "auto", label: "自动（软件编码，延迟最低）", available: true },
    { value: "x264", label: "x264（软件）", available: encoderStatus?.x264 ?? true },
    { value: "nvenc", label: "NVIDIA NVENC", available: encoderStatus?.nvenc ?? false },
    { value: "qsv", label: "Intel Quick Sync", available: encoderStatus?.qsv ?? false },
  ];

  return (
    <div className="settings-page">
      <h1>设置</h1>
      <p className="section-desc">
        修改在下次连接时生效，并保存到磁盘，下次启动继续使用。
      </p>

      {(settingsError || encoderError) && (
        <div className="setting-warning">{settingsError ?? encoderError}</div>
      )}

      <div className="settings-section">
        <h2>视频</h2>
        <div className="setting-row">
          <label htmlFor="fps">帧率</label>
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
          <label htmlFor="bitrate">码率</label>
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
          <label htmlFor="resolution">分辨率</label>
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
          <label htmlFor="encoder">编码器</label>
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
                {option.available || !encoderStatus ? option.label : `${option.label} — 不可用`}
              </option>
            ))}
          </select>
        </div>
        {encoderStatus && (
          <p className="setting-hint">
            ffmpeg: {encoderStatus.ffmpeg} · x264 {encoderStatus.x264 ? "可用" : "缺失"} · NVENC{" "}
            {encoderStatus.nvenc ? "可用" : "缺失"} · QSV{" "}
            {encoderStatus.qsv ? "可用" : "缺失"}
          </p>
        )}
      </div>

      <div className="settings-section">
        <h2>音频</h2>
        <div className="setting-row">
          <label htmlFor="audio">
            投送系统声音（WASAPI 回环；RAOP 通道以 L16 44.1 kHz 立体声发送，镜像通道以 AAC-LC 发送）
          </label>
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
        <h2>关于</h2>
        <div className="about-info">
          <p>AeroSend v0.1.0</p>
          <p>面向 Windows 的 AirPlay 投屏工具</p>
          <p className="muted">基于 Tauri + React + Rust 构建</p>
        </div>
      </div>
    </div>
  );
}
