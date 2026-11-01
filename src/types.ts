export interface Device {
  id: string;
  name: string;
  host: string;
  port: number;
  type: "airplay" | "raop";
  features: string;
  model?: string;
}

export interface StreamStats {
  /** Measured output rate of the video pump. */
  fps: number;
  /** Megabits per second actually sent. */
  bitrate: number;
  /** Capture-to-last-packet delay in milliseconds. */
  latency: number;
  resolution: string;
  codec: string;
  duration: number;
  framesCaptured: number;
  /** Capture ticks skipped because the encoder had not drained its backlog. */
  framesDropped: number;
  bytesSent: number;
  audioFramesSent: number;
  running: boolean;
  /** Set when the session ended because of a failure. */
  error: string | null;
}

export type Resolution = "native" | "1080p" | "720p" | "480p";
export type EncoderChoice = "auto" | "nvenc" | "qsv" | "x264";

export interface StreamSettings {
  fps: number;
  /** Bits per second. */
  bitrate: number;
  audio: boolean;
  resolution: Resolution;
  encoder: EncoderChoice;
  annotation: boolean;
}

export type AppState = "idle" | "discovering" | "connecting" | "streaming" | "error";

/** A DLNA renderer, as `renderers`/`scan_renderers` report it. */
export interface RendererInfo {
  id: string;
  name: string;
  model: string;
  manufacturer: string;
  baseUrl: string;
  /** It publishes AVTransport, so a cast can reach it. */
  castable: boolean;
  reachable: boolean;
  error: string | null;
  playing: boolean;
}

/** Playback on one renderer, read back from the device itself. */
export interface CastStatus {
  rendererId: string;
  title: string;
  uri: string;
  /** PLAYING | PAUSED_PLAYBACK | STOPPED | NO_MEDIA_PRESENT | unknown */
  state: string;
  seconds: number;
  totalSeconds: number;
  bytesServed: number;
  active: boolean;
  error: string | null;
}

/** A local file the media server will hand to a renderer over HTTP. */
export interface OfferedFile {
  path: string;
  title: string;
  url: string;
  mime: string;
  size: number;
}
