// 复制文本到剪贴板，返回是否成功。
// Tauri WebView 下 navigator.clipboard 可用（见 McpServiceModal 既有用法）。
export async function copyToClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
