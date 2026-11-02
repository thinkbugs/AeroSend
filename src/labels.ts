/** Device types come from discovery and stay English in code; only the badge
 *  text is localized. */
export function deviceTypeLabel(type: string): string {
  switch (type) {
    case "airplay":
      return "AirPlay 镜像";
    case "raop":
      return "AirPlay 音频";
    default:
      return type;
  }
}

/** UPnP transport states are protocol tokens; map the ones a renderer sends. */
export function castStateLabel(state: string): string {
  switch (state) {
    case "PLAYING":
      return "播放中";
    case "PAUSED_PLAYBACK":
    case "PAUSED":
      return "已暂停";
    case "STOPPED":
      return "已停止";
    case "NO_MEDIA_PRESENT":
      return "无媒体";
    case "TRANSITIONING":
      return "切换中";
    default:
      return state || "未知";
  }
}
