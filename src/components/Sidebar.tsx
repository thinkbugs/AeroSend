import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Device, AppState } from "../types";

interface SidebarProps {
  devices: Device[];
  selectedDevice: Device | null;
  state: AppState;
  scanning: boolean;
  onSelectDevice: (device: Device) => void;
  onDisconnect: () => void;
  onRescan: () => void;
  page: "dashboard" | "settings";
  onNavigate: (page: "dashboard" | "settings") => void;
}

export default function Sidebar({
  devices,
  selectedDevice,
  state,
  scanning,
  onSelectDevice,
  onDisconnect,
  onRescan,
  page,
  onNavigate,
}: SidebarProps) {
  const [time, setTime] = useState(new Date());

  useEffect(() => {
    const timer = setInterval(() => setTime(new Date()), 1000);
    return () => clearInterval(timer);
  }, []);

  return (
    <aside className="sidebar">
      <div className="sidebar-header">
        <div className="logo">
          <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <path d="M12 2L2 7l10 5 10-5-10-5z" />
            <path d="M2 17l10 5 10-5" />
            <path d="M2 12l10 5 10-5" />
          </svg>
          <span className="logo-text">AeroSend</span>
        </div>
      </div>

      <nav className="sidebar-nav">
        <button
          className={`nav-item ${page === "dashboard" ? "active" : ""}`}
          onClick={() => onNavigate("dashboard")}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <rect x="3" y="3" width="7" height="7" />
            <rect x="14" y="3" width="7" height="7" />
            <rect x="3" y="14" width="7" height="7" />
            <rect x="14" y="14" width="7" height="7" />
          </svg>
          <span>Dashboard</span>
        </button>
        <button
          className={`nav-item ${page === "settings" ? "active" : ""}`}
          onClick={() => onNavigate("settings")}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <circle cx="12" cy="12" r="3" />
            <path d="M12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42" />
          </svg>
          <span>Settings</span>
        </button>
      </nav>

      <div className="sidebar-section">
        <div className="section-header">
          <span>Devices</span>
          <button
            className={`rescan-btn ${scanning ? "scanning" : ""}`}
            onClick={onRescan}
            disabled={scanning}
            title="Rescan"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M21 2v6h-6" />
              <path d="M3 12a9 9 0 0 1 15-6.7L21 8" />
              <path d="M3 22v-6h6" />
              <path d="M21 12a9 9 0 0 1-15 6.7L3 16" />
            </svg>
          </button>
        </div>

        <div className="device-list">
          {devices.length === 0 && (
            <div className="device-empty">
              {scanning ? (
                <>
                  <div className="spinner" />
                  <span>Scanning...</span>
                </>
              ) : (
                <span>No devices found</span>
              )}
            </div>
          )}
          {devices.map((device) => (
            <button
              key={device.id}
              className={`device-item ${selectedDevice?.id === device.id ? "selected" : ""}`}
              onClick={() => onSelectDevice(device)}
              disabled={state === "streaming"}
            >
              <div className="device-icon">
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                  <rect x="2" y="3" width="20" height="14" rx="2" />
                  <path d="M8 21h8M12 17v4" />
                </svg>
              </div>
              <div className="device-info">
                <span className="device-name">{device.name}</span>
                <span className="device-host">{device.host}:{device.port}</span>
              </div>
              <span className={`device-type ${device.type}`}>{device.type}</span>
            </button>
          ))}
        </div>
      </div>

      {state === "streaming" && (
        <div className="sidebar-section">
          <button className="disconnect-btn" onClick={onDisconnect}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
            <span>Disconnect</span>
          </button>
        </div>
      )}

      <div className="sidebar-footer">
        <div className="status-indicator">
          <span className={`status-dot ${state}`} />
          <span className="status-text">
            {state === "idle" && "Ready"}
            {state === "discovering" && "Discovering..."}
            {state === "connecting" && "Connecting..."}
            {state === "streaming" && "Streaming"}
            {state === "error" && "Error"}
          </span>
        </div>
        <span className="time">{time.toLocaleTimeString("zh-CN")}</span>
      </div>
    </aside>
  );
}
