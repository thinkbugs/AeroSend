import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type Tool = "pen" | "highlighter" | "line" | "arrow" | "rect" | "ellipse" | "eraser";

interface Point {
  x: number;
  y: number;
}

interface Stroke {
  tool: Tool;
  color: string;
  width: number;
  points: Point[];
}

interface AnnotationState {
  open: boolean;
  passthrough: boolean;
}

const COLORS = ["#ff3b30", "#ff9f0a", "#ffd60a", "#34c759", "#0a84ff", "#ffffff"];
const WIDTHS = [3, 6, 12];
const HIGHLIGHTER_ALPHA = 0.35;

const TOOLS: { tool: Tool; label: string; path: string }[] = [
  { tool: "pen", label: "画笔", path: "M12 19l7-7 3 3-7 7-3-3zM18 13l-1.5-7.5L2 2l3.5 14.5L13 18l5-5z" },
  {
    tool: "highlighter",
    label: "荧光笔",
    path: "M4 20h16M6 16l8-8 4 4-8 8H6v-4z",
  },
  { tool: "line", label: "直线", path: "M4 20L20 4" },
  { tool: "arrow", label: "箭头", path: "M4 20L20 4M20 4h-8M20 4v8" },
  { tool: "rect", label: "矩形", path: "M4 5h16v14H4z" },
  { tool: "ellipse", label: "椭圆", path: "M12 5a8 7 0 1 0 0 14 8 7 0 0 0 0-14z" },
  {
    tool: "eraser",
    label: "橡皮擦",
    path: "M8 20h11M15 4l5 5-9 9H6l-2-2 11-12z",
  },
];

function isFreehand(tool: Tool): boolean {
  return tool === "pen" || tool === "highlighter" || tool === "eraser";
}

/**
 * Everything the ink layer does. Freehand strokes reach this canvas as many
 * short segments, shapes as one finished outline, so every call here is a few
 * hundred pixels rather than a full-screen repaint.
 */
function beginStyle(ctx: CanvasRenderingContext2D, stroke: Stroke) {
  ctx.save();
  ctx.lineJoin = "round";
  ctx.lineCap = "round";
  ctx.strokeStyle = stroke.color;
  ctx.fillStyle = stroke.color;
  if (stroke.tool === "highlighter") {
    // The marker layer is composited at 0.35 through CSS, so the ink itself is
    // opaque: a translucent brush drawn on its own would darken every place a
    // stroke crossed itself.
    ctx.lineWidth = stroke.width * 2.5;
  } else if (stroke.tool === "eraser") {
    // Erasing is removing pixels, not painting them black: the ink sits on a
    // transparent sheet over the desktop, so the only correct colour is none.
    ctx.globalCompositeOperation = "destination-out";
    ctx.lineWidth = stroke.width * 4;
  } else {
    ctx.lineWidth = stroke.width;
  }
}

function drawFreehand(ctx: CanvasRenderingContext2D, stroke: Stroke) {
  const points = stroke.points;
  if (!points.length) return;
  beginStyle(ctx, stroke);
  ctx.beginPath();
  ctx.moveTo(points[0].x, points[0].y);
  if (points.length === 1) {
    ctx.lineTo(points[0].x + 0.01, points[0].y);
  } else {
    for (let i = 1; i < points.length; i += 1) ctx.lineTo(points[i].x, points[i].y);
  }
  ctx.stroke();
  ctx.restore();
}

function drawSegment(ctx: CanvasRenderingContext2D, stroke: Stroke, from: Point, to: Point) {
  beginStyle(ctx, stroke);
  ctx.beginPath();
  ctx.moveTo(from.x, from.y);
  ctx.lineTo(to.x + 0.01, to.y);
  ctx.stroke();
  ctx.restore();
}

function drawShape(ctx: CanvasRenderingContext2D, stroke: Stroke) {
  const points = stroke.points;
  if (points.length < 2) return;
  const first = points[0];
  const last = points[points.length - 1];
  beginStyle(ctx, stroke);
  ctx.beginPath();
  if (stroke.tool === "line" || stroke.tool === "arrow") {
    ctx.moveTo(first.x, first.y);
    ctx.lineTo(last.x, last.y);
    ctx.stroke();
    if (stroke.tool === "arrow") {
      const angle = Math.atan2(last.y - first.y, last.x - first.x);
      const head = Math.max(14, stroke.width * 3);
      ctx.beginPath();
      ctx.moveTo(last.x, last.y);
      ctx.lineTo(last.x - head * Math.cos(angle - Math.PI / 6), last.y - head * Math.sin(angle - Math.PI / 6));
      ctx.moveTo(last.x, last.y);
      ctx.lineTo(last.x - head * Math.cos(angle + Math.PI / 6), last.y - head * Math.sin(angle + Math.PI / 6));
      ctx.stroke();
    }
  } else if (stroke.tool === "rect") {
    ctx.strokeRect(
      Math.min(first.x, last.x),
      Math.min(first.y, last.y),
      Math.abs(last.x - first.x),
      Math.abs(last.y - first.y),
    );
  } else {
    ctx.ellipse(
      (first.x + last.x) / 2,
      (first.y + last.y) / 2,
      Math.abs(last.x - first.x) / 2,
      Math.abs(last.y - first.y) / 2,
      0,
      0,
      Math.PI * 2,
    );
    ctx.stroke();
  }
  ctx.restore();
}

/** The dirty rectangle an in-progress shape can touch, arrow heads included. */
function shapeBounds(stroke: Stroke) {
  const first = stroke.points[0];
  const last = stroke.points[stroke.points.length - 1];
  const pad = Math.max(24, stroke.width * 3) + 2;
  return {
    x: Math.min(first.x, last.x) - pad,
    y: Math.min(first.y, last.y) - pad,
    width: Math.abs(last.x - first.x) + pad * 2,
    height: Math.abs(last.y - first.y) + pad * 2,
  };
}

/**
 * The ink layer: a transparent window over the monitor that is being mirrored,
 * so whatever is drawn here leaves through the screen capture and reaches the
 * receiver with the rest of the desktop.
 *
 * Three canvases, because each one costs less than repainting everything:
 *
 * - `ink` holds the opaque strokes and is only ever touched where a stroke was
 *   just drawn (a segment, a finished shape, or an erase).
 * - `marker` holds the highlighter and is composited at 0.35 by CSS, which is
 *   what makes the strokes translucent without ever blending them twice.
 * - `preview` holds the one unfinished shape; its previous bounding box is
 *   cleared instead of the whole screen.
 *
 * "Lock" is the desktop-level part: it hands the mouse back to the applications
 * underneath while the ink stays on screen.
 */
export default function AnnotationOverlay() {
  const inkRef = useRef<HTMLCanvasElement | null>(null);
  const markerRef = useRef<HTMLCanvasElement | null>(null);
  const previewRef = useRef<HTMLCanvasElement | null>(null);
  const strokesRef = useRef<Stroke[]>([]);
  const activeRef = useRef<Stroke | null>(null);
  const pointerRef = useRef<number | null>(null);
  const previewBoxRef = useRef<{ x: number; y: number; width: number; height: number } | null>(null);
  const previewFrameRef = useRef(0);
  const [tool, setTool] = useState<Tool>("pen");
  const [color, setColor] = useState(COLORS[0]);
  const [width, setWidth] = useState(WIDTHS[1]);
  const [locked, setLocked] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);

  const contextOf = useCallback((ref: React.RefObject<HTMLCanvasElement | null>) => {
    return ref.current ? ref.current.getContext("2d") : null;
  }, []);

  const clearPreview = useCallback(() => {
    const ctx = contextOf(previewRef);
    const box = previewBoxRef.current;
    previewBoxRef.current = null;
    if (!ctx || !box) return;
    ctx.clearRect(box.x, box.y, box.width, box.height);
  }, [contextOf]);

  const redrawAll = useCallback(() => {
    const ratio = window.devicePixelRatio || 1;
    for (const ref of [inkRef, markerRef, previewRef]) {
      const canvas = ref.current;
      const ctx = canvas ? canvas.getContext("2d") : null;
      if (!canvas || !ctx) continue;
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    }
    const ink = contextOf(inkRef);
    const marker = contextOf(markerRef);
    for (const stroke of strokesRef.current) {
      if (stroke.tool === "highlighter") {
        if (marker) drawFreehand(marker, stroke);
      } else if (ink) {
        if (isFreehand(stroke.tool)) drawFreehand(ink, stroke);
        else drawShape(ink, stroke);
      }
    }
    previewBoxRef.current = null;
  }, [contextOf]);

  // The overlay covers the whole monitor, so each backing store follows the
  // window: a 4 K panel and a 1080 p one both get one device pixel per screen
  // pixel of ink.
  useEffect(() => {
    const fit = () => {
      const ratio = window.devicePixelRatio || 1;
      for (const ref of [inkRef, markerRef, previewRef]) {
        const canvas = ref.current;
        if (!canvas) continue;
        canvas.width = Math.round(window.innerWidth * ratio);
        canvas.height = Math.round(window.innerHeight * ratio);
        canvas.style.width = `${window.innerWidth}px`;
        canvas.style.height = `${window.innerHeight}px`;
      }
      redrawAll();
    };
    fit();
    window.addEventListener("resize", fit);
    return () => {
      window.removeEventListener("resize", fit);
      if (previewFrameRef.current) cancelAnimationFrame(previewFrameRef.current);
    };
  }, [redrawAll]);

  // The sidebar drives the same window, so the lock follows the backend rather
  // than this window's last click.
  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      try {
        const next = await invoke<AnnotationState>("get_annotation_state");
        if (!cancelled) setLocked(next.passthrough);
      } catch {
        // One failed read; the next tick retries.
      }
    };
    void tick();
    const id = setInterval(tick, 1500);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  function flash(message: string) {
    setNote(message);
    window.setTimeout(() => setNote(null), 2600);
  }

  async function toggleLock() {
    try {
      const next = await invoke<AnnotationState>("set_annotation_passthrough", {
        enabled: !locked,
      });
      setLocked(next.passthrough);
    } catch (e) {
      flash(`锁定失败：${e}`);
    }
  }

  function undo() {
    if (strokesRef.current.pop()) {
      setDirty(strokesRef.current.length > 0);
      redrawAll();
    }
  }

  function clear() {
    strokesRef.current = [];
    activeRef.current = null;
    pointerRef.current = null;
    setDirty(false);
    redrawAll();
  }

  async function close() {
    try {
      await invoke("stop_annotation");
    } catch (e) {
      flash(`关闭失败：${e}`);
    }
  }

  async function save() {
    const ink = inkRef.current;
    const marker = markerRef.current;
    if (!ink || !marker) return;
    // One PNG that looks like the screen does: the two layers composited, with
    // the marker at the same 0.35 it is shown at.
    const out = document.createElement("canvas");
    out.width = ink.width;
    out.height = ink.height;
    const ctx = out.getContext("2d");
    if (!ctx) return;
    ctx.drawImage(ink, 0, 0);
    ctx.globalAlpha = HIGHLIGHTER_ALPHA;
    ctx.drawImage(marker, 0, 0);
    const url = out.toDataURL("image/png");
    try {
      const saved = await invoke<string | null>("save_annotation_png", {
        pngBase64: url.slice(url.indexOf(",") + 1),
      });
      if (saved) flash(`已保存 ${saved}`);
    } catch (e) {
      flash(`保存失败：${e}`);
    }
  }

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        // First escape gives the desktop back, the second one closes the sheet.
        if (locked) void toggleLock();
        else void close();
      } else if (event.key.toLowerCase() === "l" && !event.ctrlKey && !event.metaKey) {
        void toggleLock();
      } else if (event.key.toLowerCase() === "z" && (event.ctrlKey || event.metaKey)) {
        event.preventDefault();
        undo();
      } else if (event.key.toLowerCase() === "c" && !event.ctrlKey && !event.metaKey) {
        clear();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [locked]);

  const drawPreview = useCallback(() => {
    previewFrameRef.current = 0;
    const ctx = contextOf(previewRef);
    if (!ctx) return;
    const previous = previewBoxRef.current;
    if (previous) ctx.clearRect(previous.x, previous.y, previous.width, previous.height);
    previewBoxRef.current = null;
    const stroke = activeRef.current;
    if (!stroke || isFreehand(stroke.tool) || stroke.points.length < 2) return;
    drawShape(ctx, stroke);
    previewBoxRef.current = shapeBounds(stroke);
  }, [contextOf]);

  const schedulePreview = useCallback(() => {
    if (previewFrameRef.current) return;
    previewFrameRef.current = requestAnimationFrame(drawPreview);
  }, [drawPreview]);

  function pointOf(event: React.PointerEvent<HTMLCanvasElement>): Point {
    return { x: event.clientX, y: event.clientY };
  }

  /** The layer a stroke lives on; the eraser is the one that touches both. */
  function contextsFor(stroke: Stroke) {
    if (stroke.tool === "highlighter") {
      const marker = contextOf(markerRef);
      return marker ? [marker] : [];
    }
    const ink = contextOf(inkRef);
    if (stroke.tool !== "eraser") return ink ? [ink] : [];
    const marker = contextOf(markerRef);
    return ink && marker ? [ink, marker] : [];
  }

  function onPointerDown(event: React.PointerEvent<HTMLCanvasElement>) {
    if (locked || activeRef.current) return;
    if (event.pointerType === "mouse" && event.button !== 0) return;
    pointerRef.current = event.pointerId;
    try {
      event.currentTarget.setPointerCapture(event.pointerId);
    } catch {
      // Capture is an optimisation so a stroke keeps tracking past the window
      // edge; a pointer that is already gone simply does not get it.
    }
    const point = pointOf(event);
    const stroke: Stroke = { tool, color, width, points: [point] };
    activeRef.current = stroke;
    if (isFreehand(tool)) {
      for (const ctx of contextsFor(stroke)) drawSegment(ctx, stroke, point, point);
    } else {
      schedulePreview();
    }
  }

  function onPointerMove(event: React.PointerEvent<HTMLCanvasElement>) {
    const stroke = activeRef.current;
    if (!stroke || event.pointerId !== pointerRef.current) return;

    if (isFreehand(stroke.tool)) {
      const contexts = contextsFor(stroke);
      if (!contexts.length) return;
      // A fast pointer produces several positions per frame; drawing each one
      // keeps the line smooth instead of cutting corners.
      const native = event.nativeEvent;
      const samples =
        typeof native.getCoalescedEvents === "function" && native.getCoalescedEvents().length
          ? native.getCoalescedEvents()
          : [native];
      let previous = stroke.points[stroke.points.length - 1];
      for (const sample of samples) {
        const next = { x: sample.clientX, y: sample.clientY };
        if (next.x === previous.x && next.y === previous.y) continue;
        stroke.points.push(next);
        for (const ctx of contexts) drawSegment(ctx, stroke, previous, next);
        previous = next;
      }
    } else {
      stroke.points.length = 1;
      stroke.points.push(pointOf(event));
      schedulePreview();
    }
  }

  function endStroke(event?: React.PointerEvent<HTMLCanvasElement>) {
    const stroke = activeRef.current;
    if (!stroke) return;
    if (event && pointerRef.current !== null && event.pointerId !== pointerRef.current) return;
    activeRef.current = null;
    pointerRef.current = null;

    if (isFreehand(stroke.tool)) {
      strokesRef.current.push(stroke);
      setDirty(true);
      return;
    }

    if (previewFrameRef.current) {
      cancelAnimationFrame(previewFrameRef.current);
      previewFrameRef.current = 0;
    }
    clearPreview();
    const size = {
      x: Math.abs(stroke.points[0].x - stroke.points[stroke.points.length - 1].x),
      y: Math.abs(stroke.points[0].y - stroke.points[stroke.points.length - 1].y),
    };
    if (stroke.points.length < 2 || (size.x < 2 && size.y < 2)) return;
    const ink = contextOf(inkRef);
    if (!ink) return;
    drawShape(ink, stroke);
    strokesRef.current.push(stroke);
    setDirty(true);
  }

  return (
    <div className={`annotate-root ${locked ? "locked" : ""}`}>
      <canvas ref={inkRef} className="annotate-canvas" />
      <canvas ref={markerRef} className="annotate-canvas annotate-marker" />
      <canvas
        ref={previewRef}
        className="annotate-canvas annotate-preview"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endStroke}
        onPointerCancel={endStroke}
      />

      <div className="annotate-bar" role="toolbar" aria-label="批注工具">
        <div className="annotate-group">
          {TOOLS.map((item) => (
            <button
              key={item.tool}
              className={`annotate-btn ${tool === item.tool ? "active" : ""}`}
              title={item.label}
              onClick={() => setTool(item.tool)}
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d={item.path} />
              </svg>
            </button>
          ))}
        </div>

        <div className="annotate-group">
          {COLORS.map((c) => (
            <button
              key={c}
              className={`annotate-swatch ${color === c ? "active" : ""}`}
              style={{ background: c }}
              title={c}
              onClick={() => setColor(c)}
            />
          ))}
        </div>

        <div className="annotate-group">
          {WIDTHS.map((w) => (
            <button
              key={w}
              className={`annotate-btn ${width === w ? "active" : ""}`}
              title={`${w} 像素`}
              onClick={() => setWidth(w)}
            >
              <span className="annotate-dot" style={{ width: w + 4, height: w + 4 }} />
            </button>
          ))}
        </div>

        <div className="annotate-group">
          <button className="annotate-btn" onClick={undo} title="撤销（Ctrl+Z）" disabled={!dirty}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M9 14L4 9l5-5" />
              <path d="M4 9h11a5 5 0 0 1 0 10h-1" />
            </svg>
          </button>
          <button className="annotate-btn" onClick={clear} title="清空（C）" disabled={!dirty}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M3 6h18M8 6V4h8v2M6 6l1 14h10l1-14" />
            </svg>
          </button>
        </div>

        <div className="annotate-group">
          <button
            className={`annotate-btn wide ${locked ? "active" : ""}`}
            onClick={toggleLock}
            title="让鼠标穿透到桌面（L）"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <rect x="4" y="10" width="16" height="10" rx="2" />
              <path d={locked ? "M8 10V7a4 4 0 0 1 8 0" : "M8 10V7a4 4 0 0 1 8 0v3"} />
            </svg>
            <span>{locked ? "已锁定" : "锁定"}</span>
          </button>
          <button className="annotate-btn wide" onClick={save} title="将墨迹保存为 PNG">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M12 3v12M7 10l5 5 5-5" />
              <path d="M4 21h16" />
            </svg>
            <span>PNG</span>
          </button>
          <button className="annotate-btn wide close" onClick={close} title="关闭批注（Esc）">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
            <span>关闭</span>
          </button>
        </div>

        {note && <span className="annotate-note">{note}</span>}
      </div>

      {locked && (
        <p className="annotate-hint">
          墨迹保留在屏幕上，鼠标交给下层桌面。可在 AeroSend 侧栏解锁。
        </p>
      )}
    </div>
  );
}
