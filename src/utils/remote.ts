// 远端路径、文件信息与流量格式化工具（SFTP / 文件选择 / 监控面板共用）

export interface NetCounterPoint {
  ts: number;
  /** 累计接收字节数 */
  rx: number;
  /** 累计发送字节数 */
  tx: number;
}

export function joinPath(dir: string, name: string): string {
  if (dir === '/' || dir === '') return `/${name}`;
  return `${dir.replace(/\/+$/, '')}/${name}`;
}

export function baseName(p: string): string {
  const parts = p.split(/[\\/]/);
  return parts[parts.length - 1] || p;
}

/** 用单引号包裹路径，供远端 shell 直接使用（内嵌单引号转义为 '\''） */
export function shellQuote(p: string): string {
  return `'${p.replace(/'/g, "'\\''")}'`;
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

/** 当前曲线窗口内的收发累计。只使用内存计数器；重置 / 断档时 partial=true。 */
export function netWindowTotal(
  counters: NetCounterPoint[],
  start: number,
  now: number,
  maxGap = 60,
): { rx: number; tx: number; partial: boolean } | null {
  const visible = counters.filter((p) => p.ts >= start && p.ts <= now);
  if (visible.length < 2) return null;
  let rx = 0;
  let tx = 0;
  let partial = false;
  for (let i = 1; i < visible.length; i += 1) {
    const before = visible[i - 1];
    const cur = visible[i];
    if (cur.ts <= before.ts) continue;
    if (cur.ts - before.ts > maxGap) partial = true;
    if (cur.rx < before.rx || cur.tx < before.tx) {
      // 计数器重置：只能确定本次启动到当前样本的流量，重置前那段无法恢复。
      rx += cur.rx;
      tx += cur.tx;
      partial = true;
    } else {
      rx += cur.rx - before.rx;
      tx += cur.tx - before.tx;
    }
  }
  return { rx, tx, partial };
}

export function formatMtime(secs: number): string {
  const d = new Date(secs * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
