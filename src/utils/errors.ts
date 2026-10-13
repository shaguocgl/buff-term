import { readSavedLang, translate } from '../i18n';

// 把 invoke / 异常抛出的错误转成适合直接展示的文案。
// 后端错误本身已按当前语言返回，这里主要做去包装（Tauri/WebView 前缀）与空值兜底。
export function fmtError(e: unknown): string {
  const fallback = translate(readSavedLang(), 'common.opFailed');
  if (e == null) return fallback;
  let text = typeof e === 'string' ? e : e instanceof Error ? e.message : String(e);
  text = text.trim();
  const wrappers = [
    /^error invoking remote method '[^']*':\s*/i,
    /^tauri(\.\w+)*error:\s*/i,
    /^error:\s*/i,
  ];
  for (const w of wrappers) text = text.replace(w, '');
  return text || fallback;
}
