import { useAppStore } from "../stores/appStore";

export default function Settings() {
  const { settings, updateSettings } = useAppStore();

  return (
    <div className="settings-page">
      <h1>Settings</h1>

      <div className="settings-section">
        <h2>Video</h2>
        <div className="setting-row">
          <label>Target FPS</label>
          <select
            value={settings.targetFps}
            onChange={(e) => updateSettings({ targetFps: Number(e.target.value) })}
          >
            <option value={30}>30 fps</option>
            <option value={60}>60 fps</option>
          </select>
        </div>
        <div className="setting-row">
          <label>Bitrate</label>
          <select
            value={settings.bitrate}
            onChange={(e) => updateSettings({ bitrate: Number(e.target.value) })}
          >
            <option value={2000000}>2 Mbps</option>
            <option value={4000000}>4 Mbps</option>
            <option value={6000000}>6 Mbps</option>
            <option value={8000000}>8 Mbps</option>
          </select>
        </div>
        <div className="setting-row">
          <label>Resolution</label>
          <select
            value={settings.resolution}
            onChange={(e) =>
              updateSettings({ resolution: e.target.value as "native" | "1080p" | "720p" | "480p" })
            }
          >
            <option value="native">Native</option>
            <option value="1080p">1080p</option>
            <option value="720p">720p</option>
            <option value="480p">480p</option>
          </select>
        </div>
        <div className="setting-row">
          <label>Encoder</label>
          <select
            value={settings.encoder}
            onChange={(e) =>
              updateSettings({ encoder: e.target.value as "auto" | "nvenc" | "qsv" | "x264" })
            }
          >
            <option value="auto">Auto (Hardware First)</option>
            <option value="nvenc">NVIDIA NVENC</option>
            <option value="qsv">Intel Quick Sync</option>
            <option value="x264">Software (x264)</option>
          </select>
        </div>
      </div>

      <div className="settings-section">
        <h2>Audio</h2>
        <div className="setting-row">
          <label>Audio Streaming</label>
          <label className="toggle">
            <input
              type="checkbox"
              checked={settings.audioEnabled}
              onChange={(e) => updateSettings({ audioEnabled: e.target.checked })}
            />
            <span className="toggle-slider" />
          </label>
        </div>
      </div>

      <div className="settings-section">
        <h2>Annotation</h2>
        <div className="setting-row">
          <label>Enable Annotation</label>
          <label className="toggle">
            <input
              type="checkbox"
              checked={settings.annotationEnabled}
              onChange={(e) => updateSettings({ annotationEnabled: e.target.checked })}
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
