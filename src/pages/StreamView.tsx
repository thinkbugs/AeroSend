import { useState } from "react";
import type { Device, StreamStats } from "../types";

interface StreamViewProps {
  device: Device;
  stats: StreamStats | null;
}

export default function StreamView({ device, stats }: StreamViewProps) {
  const [showAnnotation, setShowAnnotation] = useState(false);
  const [muted, setMuted] = useState(false);

  return (
    <div className="stream-view">
      <div className="stream-header">
        <div className="stream-title">
          <span className="live-dot" />
          <h2>Streaming to {device.name}</h2>
        </div>
        <div className="stream-controls">
          <button
            className={`control-btn ${muted ? "active" : ""}`}
            onClick={() => setMuted(!muted)}
            title={muted ? "Unmute" : "Mute"}
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              {muted ? (
                <>
                  <line x1="1" y1="1" x2="23" y2="23" />
                  <path d="M9 9v6a3 3 0 0 0 5.12 2.12M15 9.34V4a3 3 0 0 0-5.94-.6" />
                  <path d="M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" />
                  <line x1="12" y1="19" x2="12" y2="23" />
                  <line x1="8" y1="23" x2="16" y2="23" />
                </>
              ) : (
                <>
                  <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" />
                  <path d="M19.07 4.93a10 10 0 0 1 0 14.14M15.54 8.46a5 5 0 0 1 0 7.07" />
                </>
              )}
            </svg>
          </button>
          <button
            className={`control-btn ${showAnnotation ? "active" : ""}`}
            onClick={() => setShowAnnotation(!showAnnotation)}
            title="Annotation"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M12 19l7-7 3 3-7 7-3-3z" />
              <path d="M18 13l-1.5-7.5L2 2l3.5 14.5L13 18l5-5z" />
              <path d="M2 2l7.586 7.586" />
              <circle cx="11" cy="11" r="2" />
            </svg>
          </button>
        </div>
      </div>

      <div className="stream-preview">
        <div className="preview-placeholder">
          <svg width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1" opacity="0.3">
            <rect x="2" y="3" width="20" height="14" rx="2" />
            <path d="M8 21h8M12 17v4" />
          </svg>
          <span>Screen Preview</span>
        </div>
        {showAnnotation && (
          <div className="annotation-overlay">
            <div className="annotation-toolbar">
              <button className="tool-btn active" title="Pen">
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                  <path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z" />
                </svg>
              </button>
              <button className="tool-btn" title="Eraser">
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                  <path d="M20 20H7L3 16c-.8-.8-.8-2 0-2.8L14.8 1.4c.8-.8 2-.8 2.8 0l5 5c.8.8.8 2 0 2.8L11 20" />
                  <path d="M6 11l4 4" />
                </svg>
              </button>
              <div className="color-picker">
                {["#ff3b30", "#ff9500", "#ffcc00", "#34c759", "#007aff", "#5856d6", "#af52de", "#ffffff"].map(
                  (color) => (
                    <button
                      key={color}
                      className="color-swatch"
                      style={{ backgroundColor: color }}
                    />
                  ),
                )}
              </div>
              <button className="tool-btn" title="Clear">
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                  <polyline points="3 6 5 6 21 6" />
                  <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
                </svg>
              </button>
            </div>
          </div>
        )}
      </div>

      <div className="stream-stats">
        <div className="stat-item">
          <span className="stat-label">FPS</span>
          <span className="stat-value">{stats?.fps ?? "--"}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Bitrate</span>
          <span className="stat-value">
            {stats ? `${(stats.bitrate / 1_000_000).toFixed(1)} Mbps` : "--"}
          </span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Latency</span>
          <span className="stat-value">{stats ? `${stats.latency} ms` : "--"}</span>
        </div>
        <div className="stat-item">
          <span className="stat-label">Loss</span>
          <span className="stat-value">{stats ? `${stats.packetLoss}%` : "--"}</span>
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
      </div>
    </div>
  );
}

function formatDuration(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = Math.floor(seconds % 60);
  if (h > 0) return `${h}:${m.toString().padStart(2, "0")}:${s.toString().padStart(2, "0")}`;
  return `${m}:${s.toString().padStart(2, "0")}`;
}
