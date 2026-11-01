import { create } from "zustand";
import type { StreamSettings } from "../types";

interface AppStore {
  settings: StreamSettings;
  updateSettings: (partial: Partial<StreamSettings>) => void;
  sidebarCollapsed: boolean;
  toggleSidebar: () => void;
}

export const useAppStore = create<AppStore>((set) => ({
  settings: {
    targetFps: 60,
    bitrate: 4000000,
    resolution: "native",
    encoder: "auto",
    audioEnabled: true,
    annotationEnabled: false,
  },
  updateSettings: (partial) =>
    set((state) => ({ settings: { ...state.settings, ...partial } })),
  sidebarCollapsed: false,
  toggleSidebar: () =>
    set((state) => ({ sidebarCollapsed: !state.sidebarCollapsed })),
}));
