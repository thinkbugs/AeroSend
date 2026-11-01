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
  fps: number;
  bitrate: number;
  latency: number;
  packetLoss: number;
  resolution: string;
  codec: string;
  duration: number;
}

export interface StreamSettings {
  targetFps: number;
  bitrate: number;
  resolution: "native" | "1080p" | "720p" | "480p";
  encoder: "auto" | "nvenc" | "qsv" | "x264";
  audioEnabled: boolean;
  annotationEnabled: boolean;
}

export type AppState = "idle" | "discovering" | "connecting" | "streaming" | "error";
