import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import Sidebar from "./components/Sidebar";
import Dashboard from "./pages/Dashboard";
import CastView from "./pages/CastView";
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
  const [page, setPage] = useState<"dashboard" | "cast" | "settings">("dashboard");

  useEffect(() => {
    invoke("start_discovery")
      .then(() => setScanning(true))
      .catch((e) => setError(`Discovery failed: ${e}`));
    return () => {
      invoke("stop_discovery").catch(() => undefined);
    };
  }, []);

  useEffect(() => {
    if (!scanning) return;
    let cancelled = false;
    const tick = async () => {
      try {
        const result = await invoke<Device[]>("get_devices");
        if (!cancelled) setDevices(result);
      } catch {
        // A transient read failure just means the next tick retries.
      }
    };
    tick();
    const id = setInterval(tick, 2000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [scanning]);

  // Stats only matter while a session is live; the backend reports the end of
  // the session through `running`, which stops the loop and surfaces any error.
  useEffect(() => {
    if (state !== "streaming") return;
    let cancelled = false;
    const tick = async () => {
      try {
        const stats = await invoke<StreamStats>("get_stream_stats");
        if (cancelled) return;
        setStreamStats(stats);
        if (!stats.running) {
          setState("error");
          setError(stats.error ?? "The stream ended.");
        }
      } catch {
        // Ignore one failed read; the next tick retries.
      }
    };
    tick();
    const id = setInterval(tick, 1000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [state]);

  async function rescan() {
    setScanning(false);
    await invoke("stop_discovery").catch(() => undefined);
    await invoke("start_discovery").catch(() => undefined);
    setScanning(true);
  }

  async function connectToDevice(device: Device) {
    try {
      setError(null);
      setPage("dashboard");
      await invoke("stream_start", { deviceId: device.id });
      setSelectedDevice(device);
      setState("streaming");
    } catch (e) {
      setError(`Connection failed: ${e}`);
      setState("error");
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

  return (
    <div className="app-container">
      <Sidebar
        devices={devices}
        selectedDevice={selectedDevice}
        state={state}
        scanning={scanning}
        onSelectDevice={connectToDevice}
        onDisconnect={disconnect}
        onRescan={rescan}
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
        ) : page === "cast" ? (
          <CastView onError={setError} />
        ) : state === "streaming" && selectedDevice ? (
          <StreamView device={selectedDevice} stats={streamStats} />
        ) : (
          <Dashboard devices={devices} onSelectDevice={connectToDevice} onRescan={rescan} />
        )}
      </main>
    </div>
  );
}
