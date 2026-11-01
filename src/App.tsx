import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import Sidebar from "./components/Sidebar";
import Dashboard from "./pages/Dashboard";
import StreamView from "./pages/StreamView";
import Settings from "./pages/Settings";
import type { Device, StreamStats, AppState } from "./types";

export default function App() {
  const [state, setState] = useState<AppState>("idle");
  const [devices, setDevices] = useState<Device[]>([]);
  const [selectedDevice, setSelectedDevice] = useState<Device | null>(null);
  const [streamStats, setStreamStats] = useState<StreamStats | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    startDiscovery();
    return () => {
      stopDiscovery();
    };
  }, []);

  async function startDiscovery() {
    try {
      setScanning(true);
      await invoke("start_discovery");
      pollDevices();
    } catch (e) {
      setError(`Discovery failed: ${e}`);
      setScanning(false);
    }
  }

  async function stopDiscovery() {
    try {
      await invoke("stop_discovery");
    } catch {
      // ignore
    }
    setScanning(false);
  }

  async function pollDevices() {
    try {
      const result = await invoke<Device[]>("get_devices");
      setDevices(result);
    } catch {
      // ignore
    }
    if (scanning) {
      setTimeout(pollDevices, 2000);
    }
  }

  async function connectToDevice(device: Device) {
    try {
      setError(null);
      await invoke("stream_start", { deviceId: device.id });
      setSelectedDevice(device);
      setState("streaming");
      pollStats();
    } catch (e) {
      setError(`Connection failed: ${e}`);
    }
  }

  async function disconnect() {
    try {
      await invoke("stream_stop");
      setState("idle");
      setSelectedDevice(null);
      setStreamStats(null);
    } catch (e) {
      setError(`Disconnect failed: ${e}`);
    }
  }

  async function pollStats() {
    if (state !== "streaming") return;
    try {
      const stats = await invoke<StreamStats>("get_stream_stats");
      setStreamStats(stats);
    } catch {
      // ignore
    }
    setTimeout(pollStats, 1000);
  }

  const [page, setPage] = useState<"dashboard" | "settings">("dashboard");

  return (
    <div className="app-container">
      <Sidebar
        devices={devices}
        selectedDevice={selectedDevice}
        state={state}
        scanning={scanning}
        onSelectDevice={connectToDevice}
        onDisconnect={disconnect}
        onRescan={startDiscovery}
        page={page}
        onNavigate={setPage}
      />
      <main className="main-content">
        {error && (
          <div className="error-bar">
            <span>{error}</span>
            <button onClick={() => setError(null)}>×</button>
          </div>
        )}
        {page === "settings" ? (
          <Settings />
        ) : state === "streaming" && selectedDevice ? (
          <StreamView device={selectedDevice} stats={streamStats} />
        ) : (
          <Dashboard
            devices={devices}
            onSelectDevice={connectToDevice}
            onRescan={startDiscovery}
          />
        )}
      </main>
    </div>
  );
}
