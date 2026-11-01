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

const TOOLS: { tool: Tool; label: string; path: string }[] = [
  { tool: "pen", label: "Pen", path: "M12 19l7-7 3 3-7 7-3-3zM18 13l-1.5-7.5L2 2l3.5 14.5L13 18l5-5z" },
  {
    tool: "highlighter",
    label: "Highlighter",
    path: "M4 20h16M6 16l8-8 4 4-8 8H6v-4z",
  },
  { tool: "line", label: "Straight line", path: "M4 20L20 4" },
  { tool: "arrow", label: "Arrow", path: "M4 20L20 4M20 4h-8M20 4v8" },
  { tool: "rect", label: "Rectangle", path: "M4 5h16v14H4z" },
  { tool: "ellipse", label: "Ellipse", path: "M12 5a8 7 0 1 0 0 14 8 7 0 0 0 0-14z" },
  {
    tool: "eraser",
    label: "Eraser",
    path: "M8 20h11M15 4l5 5-9 9H6l-2-2 11-12z",
  },
];

/** One stroke, drawn the way its tool behaves. */
function drawStroke(ctx: CanvasRenderingContext2D, stroke: Stroke) {
  const points = stroke.points;
  if (!points.length) return;
  ctx.save();
  ctx.lineJoin = "round";
  ctx.lineCap = "round";
  ctx.strokeStyle = stroke.color;
  ctx.lineWidth = stroke.width;
  ctx.fillStyle = stroke.color;

  if (stroke.tool === "highlighter") {
    ctx.globalAlpha = 0.35;
    ctx.lineWidth = stroke.width * 2.5;
    ctx.lineCap = "butt";
  } else if (stroke.tool === "eraser") {
    // Erasing is removing pixels, not painting them black: the ink sits on a
    // transparent sheet over the desktop, so the only correct colour is none.
    ctx.globalCompositeOperation = "destination-out";
    ctx.lineWidth = stroke.width * 4;
  }

  const first = points[0];
  const last = points[points.length - 1];

  if (stroke.tool === "pen" || stroke.tool === "highlighter" || stroke.tool === "eraser") {
    ctx.beginPath();
    ctx.moveTo(first.x, first.y);
    if (points.length === 1) {
      ctx.lineTo(first.x + 0.01, first.y);
    } else {
      for (let i = 1; i < points.length; i += 1) ctx.lineTo(points[i].x, points[i].y);
    }
    ctx.stroke();
  } else {
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
  }
  ctx.restore();
}

/**
 * The ink layer: a transparent window over the monitor that is being mirrored,
 * so whatever is drawn here leaves through the screen capture and reaches the
 * receiver with the rest of the desktop.
 *
 * "Lock" is the desktop-level part: it hands the mouse back to the applications
 * underneath while the ink stays on screen.
 */
export default function AnnotationOverlay() {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const strokesRef = useRef<Stroke[]>([]);
  const liveRef = useRef<Stroke | null>(null);
  const frameRef = useRef(0);
  const [tool, setTool] = useState<Tool>("pen");
  const [color, setColor] = useState(COLORS[0]);
  const [width, setWidth] = useState(WIDTHS[1]);
  const [locked, setLocked] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);

  const redraw = useCallback(() => {
    const canvas = canvasRef.current;
    const ctx = canvas ? canvas.getContext("2d") : null;
    if (!canvas || !ctx) return;
    const ratio = window.devicePixelRatio || 1;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.scale(ratio, ratio);
    for (const stroke of strokesRef.current) drawStroke(ctx, stroke);
    if (liveRef.current) drawStroke(ctx, liveRef.current);
  }, []);

  const scheduleRedraw = useCallback(() => {
    if (frameRef.current) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = 0;
      redraw();
    });
  }, [redraw]);

  // The overlay covers the whole monitor, so the backing store follows the
  // window rather than a fixed resolution — a 4 K panel and a 1080 p one both
  // get one device pixel per screen pixel of ink.
  useEffect(() => {
    const fit = () => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const ratio = window.devicePixelRatio || 1;
      canvas.width = Math.round(window.innerWidth * ratio);
      canvas.height = Math.round(window.innerHeight * ratio);
      canvas.style.width = `${window.innerWidth}px`;
      canvas.style.height = `${window.innerHeight}px`;
      redraw();
    };
    fit();
    window.addEventListener("resize", fit);
    return () => {
      window.removeEventListener("resize", fit);
      if (frameRef.current) cancelAnimationFrame(frameRef.current);
    };
  }, [redraw]);

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
      flash(`Lock failed: ${e}`);
    }
  }

  function undo() {
    if (strokesRef.current.pop()) {
      setDirty(strokesRef.current.length > 0);
      scheduleRedraw();
    }
  }

  function clear() {
    strokesRef.current = [];
    liveRef.current = null;
    setDirty(false);
    redraw();
  }

  async function close() {
    try {
      await invoke("stop_annotation");
    } catch (e) {
      flash(`Close failed: ${e}`);
    }
  }

  async function save() {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const url = canvas.toDataURL("image/png");
    try {
      const saved = await invoke<string | null>("save_annotation_png", {
        pngBase64: url.slice(url.indexOf(",") + 1),
      });
      if (saved) flash(`Saved ${saved}`);
    } catch (e) {
      flash(`Save failed: ${e}`);
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

  function pointOf(event: React.PointerEvent<HTMLCanvasElement>): Point {
    return { x: event.clientX, y: event.clientY };
  }

  function onPointerDown(event: React.PointerEvent<HTMLCanvasElement>) {
    if (locked) return;
    try {
      event.currentTarget.setPointerCapture(event.pointerId);
    } catch {
      // Capture is an optimisation so a stroke keeps tracking past the window
      // edge; a pointer that is already gone simply does not get it.
    }
    liveRef.current = { tool, color, width, points: [pointOf(event)] };
    scheduleRedraw();
  }

  function onPointerMove(event: React.PointerEvent<HTMLCanvasElement>) {
    if (!liveRef.current) return;
    liveRef.current.points.push(pointOf(event));
    scheduleRedraw();
  }

  function onPointerUp() {
    const stroke = liveRef.current;
    liveRef.current = null;
    if (stroke) {
      strokesRef.current.push(stroke);
      setDirty(true);
    }
    scheduleRedraw();
  }

  return (
    <div className={`annotate-root ${locked ? "locked" : ""}`}>
      <canvas
        ref={canvasRef}
        className="annotate-canvas"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
      />

      <div className="annotate-bar" role="toolbar" aria-label="Annotation tools">
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
              title={`${w} px`}
              onClick={() => setWidth(w)}
            >
              <span className="annotate-dot" style={{ width: w + 4, height: w + 4 }} />
            </button>
          ))}
        </div>

        <div className="annotate-group">
          <button className="annotate-btn" onClick={undo} title="Undo (Ctrl+Z)" disabled={!dirty}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M9 14L4 9l5-5" />
              <path d="M4 9h11a5 5 0 0 1 0 10h-1" />
            </svg>
          </button>
          <button className="annotate-btn" onClick={clear} title="Clear (C)" disabled={!dirty}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M3 6h18M8 6V4h8v2M6 6l1 14h10l1-14" />
            </svg>
          </button>
        </div>

        <div className="annotate-group">
          <button
            className={`annotate-btn wide ${locked ? "active" : ""}`}
            onClick={toggleLock}
            title="Let the mouse through to the desktop (L)"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <rect x="4" y="10" width="16" height="10" rx="2" />
              <path d={locked ? "M8 10V7a4 4 0 0 1 8 0" : "M8 10V7a4 4 0 0 1 8 0v3"} />
            </svg>
            <span>{locked ? "Locked" : "Lock"}</span>
          </button>
          <button className="annotate-btn wide" onClick={save} title="Save the ink as a PNG">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M12 3v12M7 10l5 5 5-5" />
              <path d="M4 21h16" />
            </svg>
            <span>PNG</span>
          </button>
          <button className="annotate-btn wide close" onClick={close} title="Close annotation (Esc)">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
            <span>Close</span>
          </button>
        </div>

        {note && <span className="annotate-note">{note}</span>}
      </div>

      {locked && (
        <p className="annotate-hint">
          Ink stays on screen, the desktop below takes the mouse. Unlock from AeroSend&apos;s sidebar.
        </p>
      )}
    </div>
  );
}
