import type { Device, StreamStats } from "../types";

interface StreamViewProps {
  device: Device;
  stats: StreamStats | null;
}

export default function StreamView({ device, stats }: StreamViewProps) {
  // RAOP is the speaker transport; a manual address has no advertised video
  // capability either. Both go out as audio only.
  const audioOnly = stats?.audioOnly ?? device.type === "raop";

  return (
    <div className="stream-view">
      <div className="stream-header">
        <div className="stream-title">
          <span className="live-dot" />
          <h2>
            {audioOnly ? "正在投送声音到" : "正在镜像到"} {device.name}
          </h2>
        </div>
        <div className="stream-target">{device.host}:{device.port}</div>
      </div>

      <div className="stream-stats stream-stats-grid">
        {!audioOnly && (
          <>
            <div className="stat-item">
              <span className="stat-label">帧率</span>
              <span className="stat-value">{formatNumber(stats?.fps)}</span>
            </div>
            <div className="stat-item">
              <span className="stat-label">延迟</span>
              <span className="stat-value">{stats ? `${stats.latency.toFixed(0)} ms` : "--"}</span>
            </div>
            <div className="stat-item">
              <span className="stat-label">分辨率</span>
              <span className="stat-value">{stats?.resolution ?? "--"}</span>
            </div>
          </>
        )}
        <div className="stat-item">
          <span className="stat-label">码率</span>
          <span className="stat-value">
            {stats ? `${stats.bitrate.toFixed(1)} Mbps` : "--"}
          </span>
        </div>
        <div className="stat-item">
          <span className="stat-label">时长</span>
          <span className="stat-value">
            {stats ? formatDuration(stats.duration) : "--:--"}
          </span>
        </div>
        {!audioOnly && (
          <>
            <div className="stat-item">
              <span className="stat-label">已采集帧</span>
              <span className="stat-value">{formatCount(stats?.framesCaptured)}</span>
            </div>
            <div className="stat-item">
              <span className="stat-label">丢帧</span>
              <span className="stat-value">{formatCount(stats?.framesDropped)}</span>
            </div>
            <div className="stat-item">
              <span className="stat-label">网络丢帧</span>
              <span className="stat-value">{formatCount(stats?.framesStalled)}</span>
            </div>
          </>
        )}
        <div className="stat-item">
          <span className="stat-label">音频帧</span>
          <span className="stat-value">{formatCount(stats?.audioFramesSent)}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">已发送</span>
          <span className="stat-value">
            {stats ? `${(stats.bytesSent / 1_000_000).toFixed(1)} MB` : "--"}
          </span>
        </div>
      </div>

      <div className="stream-transport">
        <span className="stat-label">传输</span>
        <span>{stats?.codec ?? (audioOnly ? "L16 / 44.1 kHz" : "H.264")}</span>
        <span className="muted">
          RTP/UDP + RTCP 发送端报告 ·{" "}
          {audioOnly
            ? "系统声音由 WASAPI 回环采集"
            : "屏幕由 DXGI 采集，声音由 WASAPI 回环采集"}
        </span>
      </div>
    </div>
  );
}

function formatNumber(value: number | undefined): string {
  return value === undefined ? "--" : value.toFixed(1);
}

function formatCount(value: number | undefined): string {
  return value === undefined ? "--" : value.toLocaleString();
}

function formatDuration(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = Math.floor(seconds % 60);
  if (h > 0) return `${h}:${m.toString().padStart(2, "0")}:${s.toString().padStart(2, "0")}`;
  return `${m}:${s.toString().padStart(2, "0")}`;
}
