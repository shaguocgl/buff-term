// 把 invoke / 异常抛出的错误转成适合直接展示的文案。
// 后端错误本身已是中文，这里主要做去包装（Tauri/WebView 前缀）与空值兜底。
export function fmtError(e: unknown): string {
  if (e == null) return '操作失败';
  let text = typeof e === 'string' ? e : e instanceof Error ? e.message : String(e);
  text = text.trim();
  const wrappers = [
    /^error invoking remote method '[^']*':\s*/i,
    /^tauri(\.\w+)*error:\s*/i,
    /^error:\s*/i,
  ];
  for (const w of wrappers) text = text.replace(w, '');
  return text || '操作失败';
}
