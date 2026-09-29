// 平台判定统一用 navigator.platform，不能用 userAgent：
// tauri.conf.json 为窗口强制设置了 macOS Safari 的 UA（所有平台生效），
// 用 UA 判断会把 Windows / Linux 误判成 macOS。xterm 内部同样以 navigator.platform 判定。
export const IS_MAC = /^Mac/i.test(navigator.platform);
