import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { StreamSettings } from "../types";

const DEFAULTS: StreamSettings = {
  fps: 30,
  bitrate: 4_000_000,
  audio: true,
  resolution: "native",
  encoder: "auto",
  annotation: false,
};

interface AppStore {
  /** Mirrors the Rust-side settings; the backend is the source of truth. */
  settings: StreamSettings;
  settingsLoaded: boolean;
  settingsError: string | null;
  loadSettings: () => Promise<void>;
  updateSettings: (partial: Partial<StreamSettings>) => Promise<void>;
  sidebarCollapsed: boolean;
  toggleSidebar: () => void;
}

export const useAppStore = create<AppStore>((set, get) => ({
  settings: DEFAULTS,
  settingsLoaded: false,
  settingsError: null,

  async loadSettings() {
    try {
      const loaded = await invoke<StreamSettings>("get_stream_settings");
      set({ settings: loaded, settingsLoaded: true, settingsError: null });
    } catch (e) {
      set({ settingsError: `Could not read settings: ${e}`, settingsLoaded: true });
    }
  },

  async updateSettings(partial) {
    const next = { ...get().settings, ...partial };
    set({ settings: next, settingsError: null });
    try {
      await invoke("set_stream_settings", { settings: next });
    } catch (e) {
      set({ settingsError: `Not saved: ${e}` });
    }
  },

  sidebarCollapsed: false,
  toggleSidebar: () => set((state) => ({ sidebarCollapsed: !state.sidebarCollapsed })),
}));
