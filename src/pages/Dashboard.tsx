import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Device } from "../types";
import { deviceTypeLabel } from "../labels";

interface DashboardProps {
  devices: Device[];
  onSelectDevice: (device: Device) => void;
  onRescan: () => void;
}

export default function Dashboard({ devices, onSelectDevice, onRescan }: DashboardProps) {
  const [manualHost, setManualHost] = useState("");
  const [manualPort, setManualPort] = useState("7000");
  const [manualName, setManualName] = useState("");
  const [adding, setAdding] = useState(false);

  async function handleAddManual() {
    if (!manualHost.trim()) return;
    setAdding(true);
    try {
      await invoke("add_manual_device", {
        host: manualHost.trim(),
        port: parseInt(manualPort) || 7000,
        name: manualName.trim() || undefined,
      });
      setManualHost("");
      setManualPort("7000");
      setManualName("");
      onRescan();
    } catch (e) {
      console.error("Failed to add device:", e);
    }
    setAdding(false);
  }

  return (
    <div className="dashboard">
      <div className="dashboard-hero">
        <h1>AeroSend</h1>
        <p>面向 Windows 的 AirPlay 投屏工具</p>
        <div className="hero-actions">
          <button className="btn-primary" onClick={onRescan}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M21 2v6h-6" />
              <path d="M3 12a9 9 0 0 1 15-6.7L21 8" />
              <path d="M3 22v-6h6" />
              <path d="M21 12a9 9 0 0 1-15 6.7L3 16" />
            </svg>
            扫描设备
          </button>
        </div>
      </div>

      <div className="dashboard-grid">
        <div className="stat-card">
          <div className="stat-icon blue">
            <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <rect x="2" y="3" width="20" height="14" rx="2" />
              <path d="M8 21h8M12 17v4" />
            </svg>
          </div>
          <div className="stat-info">
            <span className="stat-value">{devices.length}</span>
            <span className="stat-label">已发现设备</span>
          </div>
        </div>

        <div className="stat-card">
          <div className="stat-icon green">
            <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M22 11.08V12a10 10 0 1 1-5.93-9.14" />
              <polyline points="22 4 12 14.01 9 11.01" />
            </svg>
          </div>
          <div className="stat-info">
            <span className="stat-value">就绪</span>
            <span className="stat-label">系统状态</span>
          </div>
        </div>

        <div className="stat-card">
          <div className="stat-icon purple">
            <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M12 2L2 7l10 5 10-5-10-5z" />
              <path d="M2 17l10 5 10-5" />
              <path d="M2 12l10 5 10-5" />
            </svg>
          </div>
          <div className="stat-info">
            <span className="stat-value">H.264</span>
            <span className="stat-label">视频编码</span>
          </div>
        </div>

        <div className="stat-card">
          <div className="stat-icon orange">
            <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M5 12.55a11 11 0 0 1 14.08 0" />
              <path d="M1.42 9a16 16 0 0 1 21.16 0" />
              <path d="M8.53 16.11a6 6 0 0 1 6.95 0" />
              <line x1="12" y1="20" x2="12.01" y2="20" />
            </svg>
          </div>
          <div className="stat-info">
            <span className="stat-value">AirPlay</span>
            <span className="stat-label">投屏协议</span>
          </div>
        </div>
      </div>

      <div className="manual-device-section">
        <h2>手动添加设备</h2>
        <p className="section-desc">输入局域网内 AirPlay 接收端的 IP 地址。</p>
        <div className="manual-device-form">
          <input
            type="text"
            placeholder="IP 地址（例如 192.168.1.100）"
            value={manualHost}
            onChange={(e) => setManualHost(e.target.value)}
            className="form-input"
          />
          <input
            type="number"
            placeholder="端口"
            value={manualPort}
            onChange={(e) => setManualPort(e.target.value)}
            className="form-input form-input-sm"
          />
          <input
            type="text"
            placeholder="名称（可选）"
            value={manualName}
            onChange={(e) => setManualName(e.target.value)}
            className="form-input"
          />
          <button
            className="btn-primary"
            onClick={handleAddManual}
            disabled={adding || !manualHost.trim()}
          >
            {adding ? "添加中…" : "添加"}
          </button>
        </div>
      </div>

      {devices.length > 0 && (
        <div className="device-grid-section">
          <h2>可用设备</h2>
          <div className="device-grid">
            {devices.map((device) => (
              <button
                key={device.id}
                className="device-card"
                onClick={() => onSelectDevice(device)}
              >
                <div className="device-card-icon">
                  <svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
                    <rect x="2" y="3" width="20" height="14" rx="2" />
                    <path d="M8 21h8M12 17v4" />
                  </svg>
                </div>
                <div className="device-card-info">
                  <span className="device-card-name">{device.name}</span>
                  <span className="device-card-detail">
                    {device.host}:{device.port} · {deviceTypeLabel(device.type)}
                  </span>
                </div>
                <div className="device-card-action">
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <path d="M5 12h14M12 5l7 7-7 7" />
                  </svg>
                </div>
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
