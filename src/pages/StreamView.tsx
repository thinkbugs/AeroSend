import type { Device, StreamStats } from "../types";

interface StreamViewProps {
  device: Device;
  stats: StreamStats | null;
}

export default function StreamView({ device, stats }: StreamViewProps) {
  return (
    <div className="stream-view">
      <div className="stream-header">
        <div className="stream-title">
          <span className="live-dot" />
          <h2>Streaming to {device.name}</h2>
        </div>
        <div className="stream-target">{device.host}:{device.port}</div>
      </div>

      <div className="stream-stats stream-stats-grid">
        <div className="stat-item">
          <span className="stat-label">FPS</span>
          <span className="stat-value">{formatNumber(stats?.fps)}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Bitrate</span>
          <span className="stat-value">
            {stats ? `${stats.bitrate.toFixed(1)} Mbps` : "--"}
          </span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Latency</span>
          <span className="stat-value">{stats ? `${stats.latency.toFixed(0)} ms` : "--"}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Resolution</span>
          <span className="stat-value">{stats?.resolution ?? "--"}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Duration</span>
          <span className="stat-value">
            {stats ? formatDuration(stats.duration) : "--:--"}
          </span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Frames captured</span>
          <span className="stat-value">{formatCount(stats?.framesCaptured)}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Dropped</span>
          <span className="stat-value">{formatCount(stats?.framesDropped)}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Audio frames</span>
          <span className="stat-value">{formatCount(stats?.audioFramesSent)}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Sent</span>
          <span className="stat-value">
            {stats ? `${(stats.bytesSent / 1_000_000).toFixed(1)} MB` : "--"}
          </span>
        </div>
      </div>

      <div className="stream-transport">
        <span className="stat-label">Transport</span>
        <span>{stats?.codec ?? "H.264 / AAC"}</span>
        <span className="muted">
          RTP/UDP with RTCP sender reports · screen captured with DXGI, audio with WASAPI loopback
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
