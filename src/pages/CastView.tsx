import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { CastStatus, OfferedFile, RendererInfo } from "../types";
import { castStateLabel } from "../labels";

interface CastViewProps {
  /** Failures are shown in the window-wide bar the app already owns. */
  onError: (message: string) => void;
}

/**
 * The media cast page: pick a renderer, hand it a video, a photo, a song or a
 * URL, and drive its transport. Every control here calls one registered
 * command, and the numbers shown are the device's own — position comes from
 * GetPositionInfo at 1 Hz, volume from GetVolume, so a TV remote stays honest
 * with this screen.
 */
export default function CastView({ onError }: CastViewProps) {
  const [renderers, setRenderers] = useState<RendererInfo[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [status, setStatus] = useState<CastStatus | null>(null);
  const [offered, setOffered] = useState<OfferedFile[]>([]);
  const [scanning, setScanning] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [address, setAddress] = useState("");
  const [streamUrl, setStreamUrl] = useState("");
  const [streamTitle, setStreamTitle] = useState("");
  const [volume, setVolume] = useState(50);
  /** Where the slider is being dragged to; null when it is tracking the device. */
  const [draftSeek, setDraftSeek] = useState<number | null>(null);
  const selectedIdRef = useRef<string | null>(null);
  selectedIdRef.current = selectedId;

  async function refreshRenderers() {
    try {
      const list = await invoke<RendererInfo[]>("renderers");
      setRenderers(list);
      setSelectedId((current) => {
        if (current && list.some((r) => r.id === current)) return current;
        // Open on something usable rather than on an empty panel.
        return list.find((r) => r.castable)?.id ?? null;
      });
    } catch (e) {
      onError(String(e));
    }
  }

  async function refreshOffered() {
    try {
      setOffered(await invoke<OfferedFile[]>("get_offered_files"));
    } catch {
      // The list is decoration; a failed read leaves the previous one.
    }
  }

  useEffect(() => {
    refreshRenderers();
    refreshOffered();
    const id = setInterval(refreshRenderers, 2000);
    return () => clearInterval(id);
  }, []);

  // Position only matters while we hold a session on the selected renderer.
  useEffect(() => {
    if (!selectedId) {
      setStatus(null);
      return;
    }
    setDraftSeek(null);
    let cancelled = false;
    const tick = async () => {
      try {
        const next = await invoke<CastStatus | null>("get_cast_status", {
          rendererId: selectedId,
        });
        if (!cancelled) setStatus(next);
      } catch {
        // One missed poll is not worth an error bar.
      }
    };
    tick();
    const id = setInterval(tick, 1000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [selectedId]);

  // The device's own volume, asked once per selection.
  useEffect(() => {
    if (!selectedId) return;
    invoke<number>("get_renderer_volume", { rendererId: selectedId })
      .then(setVolume)
      .catch(() => undefined);
  }, [selectedId]);

  const selected = renderers.find((r) => r.id === selectedId) ?? null;

  async function run<T>(label: string, fn: () => Promise<T>): Promise<T | null> {
    setBusy(label);
    try {
      return await fn();
    } catch (e) {
      onError(`${label}: ${e}`);
      return null;
    } finally {
      setBusy(null);
    }
  }

  async function scan() {
    setScanning(true);
    try {
      const list = await invoke<RendererInfo[]>("scan_renderers");
      setRenderers(list);
      setSelectedId((current) =>
        current && list.some((r) => r.id === current)
          ? current
          : (list.find((r) => r.castable)?.id ?? null),
      );
    } catch (e) {
      onError(`扫描失败：${e}`);
    }
    setScanning(false);
  }

  async function addByAddress() {
    const location = address.trim();
    if (!location) return;
    const added = await run("添加渲染器", () =>
      invoke<RendererInfo>("add_renderer", { location }),
    );
    if (!added) return;
    setAddress("");
    setSelectedId(added.id);
    refreshRenderers();
  }

  async function forget(rendererId: string) {
    await run("移除渲染器", () =>
      invoke<boolean>("remove_renderer", { rendererId }),
    );
    if (selectedIdRef.current === rendererId) setSelectedId(null);
    refreshRenderers();
  }

  async function castPickedFile() {
    if (!selectedId) return;
    const path = await run("打开文件", () => invoke<string | null>("pick_media_file"));
    if (!path) return; // cancelled
    const next = await run("投送文件", () =>
      invoke<CastStatus>("cast_file", { rendererId: selectedId, path }),
    );
    if (next) {
      setStatus(next);
      refreshOffered();
    }
  }

  async function castStream() {
    if (!selectedId || !streamUrl.trim()) return;
    const next = await run("投送 URL", () =>
      invoke<CastStatus>("cast_url", {
        rendererId: selectedId,
        url: streamUrl.trim(),
        title: streamTitle.trim() || streamUrl.trim(),
      }),
    );
    if (next) setStatus(next);
  }

  async function transport(action: "Play" | "Pause" | "Stop") {
    if (!selectedId) return;
    const actionLabel = action === "Play" ? "播放" : action === "Pause" ? "暂停" : "停止";
    const next = await run(actionLabel, () =>
      invoke<CastStatus>("renderer_action", { rendererId: selectedId, action }),
    );
    if (next) setStatus(next);
  }

  async function commitSeek(seconds: number) {
    if (!selectedId) return;
    const next = await run("跳转", () =>
      invoke<CastStatus>("renderer_seek", { rendererId: selectedId, seconds }),
    );
    if (next) setStatus(next);
  }

  async function commitVolume(value: number) {
    if (!selectedId) return;
    await run("音量", () =>
      invoke<void>("set_renderer_volume", { rendererId: selectedId, volume: value }),
    );
  }

  const playing = status?.state === "PLAYING";
  const hasSession = Boolean(status?.active);
  const total = status?.totalSeconds ?? 0;
  const shownPosition = Math.round(draftSeek ?? status?.seconds ?? 0);
  /** What is on the renderer, read off the address we sent it. */
  const kind = status ? mediaKindOf(status.uri) : null;

  return (
    <div className="cast-page">
      <div className="cast-head">
        <div>
          <h1>媒体投屏</h1>
          <p className="section-desc">
            把本地的视频、图片或音乐交给局域网内的渲染器播放。渲染器自己通过 HTTP 拉流，
            按自己的时钟播放，拖动进度即时生效。
          </p>
        </div>
        <button className="btn-primary" onClick={scan} disabled={scanning}>
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <path d="M21 2v6h-6" />
            <path d="M3 12a9 9 0 0 1 15-6.7L21 8" />
            <path d="M3 22v-6h6" />
            <path d="M21 12a9 9 0 0 1-15 6.7L3 16" />
          </svg>
          {scanning ? "正在扫描…" : "扫描渲染器"}
        </button>
      </div>

      <div className="cast-layout">
        <section className="cast-renderers">
          <h2>渲染器</h2>
          {renderers.length === 0 && (
            <p className="device-empty">
              没有收到 SSDP 应答。可在下方按地址手动添加——不少网络屏蔽组播，
              但能连通的渲染器照样可以投屏。
            </p>
          )}
          {renderers.map((renderer) => (
            <div
              key={renderer.id}
              className={`renderer-row ${renderer.id === selectedId ? "selected" : ""}`}
            >
              <button
                className="renderer-pick"
                onClick={() => setSelectedId(renderer.id)}
                disabled={!renderer.castable}
                title={renderer.error ?? renderer.baseUrl}
              >
                <span className="renderer-name">{renderer.name || renderer.baseUrl}</span>
                <span className="renderer-detail">{rendererDetail(renderer)}</span>
                <span className="renderer-badges">
                  <span className={`renderer-badge ${renderer.castable ? "ok" : "no"}`}>
                    {renderer.castable ? "可投屏" : "非播放器"}
                  </span>
                  {renderer.playing && <span className="renderer-badge live">播放中</span>}
                  {!renderer.reachable && <span className="renderer-badge no">无法连接</span>}
                </span>
              </button>
              <button
                className="renderer-forget"
                onClick={() => forget(renderer.id)}
                title="移除该渲染器"
              >
                ×
              </button>
            </div>
          ))}

          <div className="cast-add">
            <label htmlFor="renderer-address">按地址添加</label>
            <div className="cast-add-row">
              <input
                id="renderer-address"
                className="form-input"
                placeholder="192.168.1.9:8290"
                title="渲染器的 ip:端口，或其设备描述的完整 URL"
                value={address}
                onChange={(e) => setAddress(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && addByAddress()}
              />
              <button className="btn-secondary" onClick={addByAddress} disabled={!address.trim()}>
                添加
              </button>
            </div>
          </div>
        </section>

        <section className="cast-panel">
          {!selected ? (
            <p className="device-empty">选择一个渲染器开始投屏。</p>
          ) : (
            <>
              <div className="cast-panel-head">
                <h2>{selected.name || selected.baseUrl}</h2>
                <span className="muted">{selected.baseUrl}</span>
              </div>

              {hasSession && status ? (
                <div className="cast-now">
                  <div className="cast-now-title">
                    <span className="live-dot" />
                    <strong>{status.title}</strong>
                    {kind && <span className="renderer-badge">{KIND_LABEL[kind]}</span>}
                    <span className={`cast-state ${playing ? "playing" : ""}`}>{castStateLabel(status.state)}</span>
                  </div>
                  {kind === "image" ? (
                    // No timeline to offer: a photo has no position, and a
                    // slider stuck at 0:00 reads as a broken player.
                    <p className="setting-hint">
                      图片没有播放进度；渲染器显示期间可停止或改投其他文件。
                    </p>
                  ) : (
                  <div className="cast-progress">
                    <span className="cast-time">{formatTime(shownPosition)}</span>
                    <input
                      type="range"
                      className="cast-slider"
                      min={0}
                      max={Math.max(total, 1)}
                      step={1}
                      value={shownPosition}
                      disabled={total <= 0}
                      onChange={(e) => setDraftSeek(Number(e.target.value))}
                      onPointerUp={() => {
                        if (draftSeek !== null) commitSeek(draftSeek);
                        setDraftSeek(null);
                      }}
                    />
                    <span className="cast-time">{formatTime(total)}</span>
                  </div>
                  )}
                  <div className="cast-transport">
                    <button
                      className="control-btn"
                      onClick={() => transport("Play")}
                      disabled={playing || busy !== null}
                      title={hasSession ? "继续" : "播放"}
                    >
                      <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
                        <path d="M8 5v14l11-7z" />
                      </svg>
                    </button>
                    <button
                      className="control-btn"
                      onClick={() => transport("Pause")}
                      disabled={!playing || busy !== null}
                      title="暂停"
                    >
                      <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
                        <rect x="6" y="5" width="4" height="14" />
                        <rect x="14" y="5" width="4" height="14" />
                      </svg>
                    </button>
                    <button
                      className="control-btn"
                      onClick={() => transport("Stop")}
                      disabled={busy !== null}
                      title="停止"
                    >
                      <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
                        <rect x="6" y="6" width="12" height="12" />
                      </svg>
                    </button>
                    <label className="cast-volume">
                      <span>音量</span>
                      <input
                        type="range"
                        min={0}
                        max={100}
                        value={volume}
                        onChange={(e) => setVolume(Number(e.target.value))}
                        onPointerUp={(e) => commitVolume(Number(e.currentTarget.value))}
                      />
                      <span className="cast-time">{volume}%</span>
                    </label>
                  </div>
                  <p className="muted cast-served">
                    {formatBytes(status.bytesServed)} 已从本机送出 · {status.uri}
                  </p>
                </div>
              ) : (
                <p className="setting-hint">
                  {selected.castable
                    ? "空闲。选择一个视频、图片或音乐文件，或粘贴流地址。"
                    : "该设备未提供 AVTransport 服务，无法投屏。"}
                </p>
              )}

              <div className="cast-actions">
                <button
                  className="btn-primary"
                  onClick={castPickedFile}
                  disabled={!selected.castable || busy !== null}
                >
                  选择媒体文件投屏…
                </button>
                <span className="muted">
                  视频、图片与音乐 · 使用 Windows 原生文件选择器，由内置媒体服务器提供流
                </span>
              </div>

              <div className="cast-url">
                <label htmlFor="stream-url">渲染器可访问的流地址</label>
                <div className="cast-add-row">
                  <input
                    id="stream-url"
                    className="form-input"
                    placeholder="http://192.168.1.20:8080/live.mp4"
                    value={streamUrl}
                    onChange={(e) => setStreamUrl(e.target.value)}
                  />
                  <input
                    className="form-input form-input-sm"
                    placeholder="标题"
                    value={streamTitle}
                    onChange={(e) => setStreamTitle(e.target.value)}
                  />
                  <button
                    className="btn-secondary"
                    onClick={castStream}
                    disabled={!selected.castable || !streamUrl.trim()}
                  >
                    投送 URL
                  </button>
                </div>
              </div>
            </>
          )}

          {offered.length > 0 && (
            <div className="cast-offered">
              <h2>正在提供</h2>
              {offered.map((file) => (
                <div className="cast-offered-row" key={file.url}>
                  <div className="cast-offered-info">
                    <span className="renderer-name">{file.title}</span>
                    <span className="renderer-detail">
                      {file.mime} · {formatBytes(file.size)} · {file.url}
                    </span>
                  </div>
                </div>
              ))}
            </div>
          )}
        </section>
      </div>
    </div>
  );
}

/// The address, plus the model only when the device calls itself something
/// other than that — many renderers repeat the friendly name in both fields.
function rendererDetail(renderer: RendererInfo): string {
  const extra = renderer.model && renderer.model !== renderer.name ? renderer.model : null;
  return extra ? `${renderer.baseUrl} · ${extra}` : renderer.baseUrl;
}

/// What kind of media an address points at, from the extension the URL spells
/// out. A served file carries its own extension for exactly this reason, and a
/// photo has no position to seek to — showing a timeline for one says "broken
/// player" when the device is doing precisely what it should.
function mediaKindOf(uri: string): MediaKind {
  const path = uri.split(/[?#]/)[0] ?? uri;
  const dot = path.lastIndexOf(".");
  const ext = dot < 0 ? "" : path.slice(dot + 1).toLowerCase();
  if (["jpg", "jpeg", "png", "gif", "webp", "bmp", "avif", "heic", "heif"].includes(ext)) {
    return "image";
  }
  if (["mp3", "m4a", "aac", "flac", "wav", "ogg", "oga", "opus", "wma"].includes(ext)) {
    return "audio";
  }
  return "video";
}

type MediaKind = "video" | "audio" | "image";

const KIND_LABEL: Record<MediaKind, string> = { video: "视频", audio: "音乐", image: "图片" };

function formatTime(seconds: number): string {  const total = Math.max(0, Math.floor(seconds));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => n.toString().padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

function formatBytes(bytes: number): string {
  if (bytes < 1000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1000).toFixed(1)} kB`;
  if (bytes < 1_000_000_000) return `${(bytes / 1_000_000).toFixed(1)} MB`;
  return `${(bytes / 1_000_000_000).toFixed(2)} GB`;
}
