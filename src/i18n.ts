export type Language = "zh_cn" | "en";

export const messages = {
  zh_cn: {
    product: "剪贴板助手", settings: "设置", general: "常规", quickPanel: "快速剪贴板",
    savedHistory: "已保存的历史", searchPlaceholder: "搜索内容或备注", searchAria: "搜索剪贴板",
    emptyHistory: "还没有剪贴内容", emptyHistoryDetail: "新复制的内容会显示在这里。",
    noMatches: "没有匹配的内容", noMatchesDetail: "请尝试其他关键词。",
    historyUnavailable: "剪贴板历史暂时不可用", pasteSent: "已发送粘贴命令",
    copyOnly: "无法安全粘贴，内容已复制，请手动粘贴。", pasteFailed: "粘贴请求失败",
    noteSaveFailed: "备注保存失败", enterToPaste: "Enter 粘贴", escToClose: "Esc 关闭",
    imageItem: "图片剪贴内容", clipboardItem: "剪贴内容", unknownApp: "未知应用",
    containsImage: "包含图片", addNote: "添加备注", noteFor: (value: string) => `${value}的备注`, now: "刚刚",
    language: "语言", languageDetail: "管理页和快速剪贴板的显示语言", chinese: "简体中文", english: "English",
    storage: "本地保存", retention: "保存期限", retentionDetail: "到期内容会被删除；所有期限仍受本地容量安全上限限制",
    oneDay: "1 天", sevenDays: "7 天", thirtyDays: "30 天", ninetyDays: "90 天", forever: "永久（无时间期限，仍受本地容量安全上限限制）",
    storageAvailable: "本地保存可用", storageUnavailable: "本地保存不可用，当前仅保留本次运行内容",
    shortcuts: "快捷键", togglePanel: "显示或隐藏快速剪贴板", shortcutAvailable: "快捷键可用",
    shortcutConflict: "快捷键已被其他程序占用", shortcutUnavailable: "快捷键不可用",
    appearance: "外观与其他设置", comingSoon: "即将支持", startup: "启动设置", sound: "剪贴提示音",
  },
  en: {
    product: "Clipboard Assistant", settings: "Settings", general: "General", quickPanel: "Quick clipboard",
    savedHistory: "Saved history", searchPlaceholder: "Search text or notes", searchAria: "Search clipboard",
    emptyHistory: "No clipboard history yet", emptyHistoryDetail: "New clipboard items will appear here.",
    noMatches: "No matching clips", noMatchesDetail: "Try a different search.",
    historyUnavailable: "Clipboard history is unavailable", pasteSent: "Paste command sent",
    copyOnly: "Cannot paste safely; content was copied. Paste it manually.", pasteFailed: "Paste request failed",
    noteSaveFailed: "Note was not saved", enterToPaste: "Enter to paste", escToClose: "Esc to close",
    imageItem: "Image clipboard item", clipboardItem: "Clipboard item", unknownApp: "Unknown app",
    containsImage: "Contains image", addNote: "Add a note", noteFor: (value: string) => `Note for ${value}`, now: "Now",
    language: "Language", languageDetail: "Display language for settings and the quick panel", chinese: "简体中文", english: "English",
    storage: "Local storage", retention: "Keep history for", retentionDetail: "Expired items are removed; every period remains subject to local capacity safety limits",
    oneDay: "1 day", sevenDays: "7 days", thirtyDays: "30 days", ninetyDays: "90 days", forever: "Forever (no time limit; local capacity limits still apply)",
    storageAvailable: "Local storage is available", storageUnavailable: "Local storage is unavailable; history is session-only",
    shortcuts: "Shortcuts", togglePanel: "Show or hide quick clipboard", shortcutAvailable: "Shortcut is available",
    shortcutConflict: "Shortcut is used by another application", shortcutUnavailable: "Shortcut is unavailable",
    appearance: "Appearance and other settings", comingSoon: "Coming soon", startup: "Startup settings", sound: "Clipboard sound",
  },
} as const;

export type Dictionary = (typeof messages)[Language];
export const dictionary = (language: Language): Dictionary => messages[language];
