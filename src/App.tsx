import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import Sidebar from "./components/Sidebar";
import Dashboard from "./pages/Dashboard";
import CastView from "./pages/CastView";
import StreamView from "./pages/StreamView";
import Settings from "./pages/Settings";
import type { AnnotationState, Device, StreamStats, AppState } from "./types";

interface AnnotationStateInfo {
  open: boolean;
  passthrough: boolean;
}

export default function App() {
  const [state, setState] = useState<AppState>("idle");
  const [devices, setDevices] = useState<Device[]>([]);
  const [selectedDevice, setSelectedDevice] = useState<Device | null>(null);
  const [streamStats, setStreamStats] = useState<StreamStats | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [page, setPage] = useState<"dashboard" | "cast" | "settings">("dashboard");
  const [annotation, setAnnotation] = useState<AnnotationStateInfo>({
    open: false,
    passthrough: false,
  });

  useEffect(() => {
    invoke("start_discovery")
      .then(() => setScanning(true))
      .catch((e) => setError(`设备发现失败：${e}`));
    return () => {
      invoke("stop_discovery").catch(() => undefined);
    };
  }, []);

  // The overlay is a desktop window of its own: its toolbar can lock the pointer
  // and the window can be closed outside this one. While it is open the sidebar
  // reads the truth from the backend instead of trusting its last click.
  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      try {
        const next = await invoke<AnnotationState>("get_annotation_state");
        if (!cancelled) setAnnotation(next);
      } catch {
        // One failed read; the next tick retries.
      }
    };
    void tick();
    if (!annotation.open) {
      return () => {
        cancelled = true;
      };
    }
    const id = setInterval(tick, 1500);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [annotation.open]);

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
          setError(stats.error ?? "投屏会话已结束。");
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
      setError(`连接失败：${e}`);
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
      setError(`断开连接失败：${e}`);
    }
  }

  async function toggleAnnotation() {
    try {
      const next = annotation.open
        ? await invoke<AnnotationState>("stop_annotation")
        : await invoke<AnnotationState>("start_annotation");
      setAnnotation(next);
    } catch (e) {
      setError(`启动批注失败：${e}`);
    }
  }

  async function toggleAnnotationLock() {
    try {
      const next = await invoke<AnnotationState>("set_annotation_passthrough", {
        enabled: !annotation.passthrough,
      });
      setAnnotation(next);
    } catch (e) {
      setError(`切换鼠标穿透失败：${e}`);
    }
  }

  return (
    <div className="app-container">
      <Sidebar
        devices={devices}
        selectedDevice={selectedDevice}
        state={state}
        scanning={scanning}
        annotation={annotation}
        onSelectDevice={connectToDevice}
        onDisconnect={disconnect}
        onRescan={rescan}
        onToggleAnnotation={toggleAnnotation}
        onToggleAnnotationLock={toggleAnnotationLock}
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
