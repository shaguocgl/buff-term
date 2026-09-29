// 远端路径与文件信息格式化工具（SFTP 面板与文件选择弹窗共用）

export function joinPath(dir: string, name: string): string {
  if (dir === '/' || dir === '') return `/${name}`;
  return `${dir.replace(/\/+$/, '')}/${name}`;
}

export function baseName(p: string): string {
  const parts = p.split(/[\\/]/);
  return parts[parts.length - 1] || p;
}

/** 返回父目录路径，根目录返回自身 */
export function parentPath(dir: string): string {
  if (dir === '/' || dir === '') return '/';
  return dir.replace(/\/[^/]*$/, '') || '/';
}

export function formatBytes(n: number): string {
  if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${n} B`;
}

export function formatMtime(secs: number): string {
  const d = new Date(secs * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
