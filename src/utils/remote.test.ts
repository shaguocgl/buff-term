import { describe, expect, it } from 'vitest';
import { baseName, formatBytes, joinPath, netWindowTotal, parentPath, shellQuote } from './remote';

describe('remote path helpers', () => {
  it('joins paths without duplicate separators', () => {
    expect(joinPath('/', 'etc')).toBe('/etc');
    expect(joinPath('/etc/', 'nginx.conf')).toBe('/etc/nginx.conf');
    expect(joinPath('/etc', 'nginx.conf')).toBe('/etc/nginx.conf');
    expect(joinPath('', 'a')).toBe('/a');
  });

  it('returns the last component for unix and windows paths', () => {
    expect(baseName('/var/log/syslog')).toBe('syslog');
    expect(baseName('C:\\Users\\me\\id_rsa')).toBe('id_rsa');
    expect(baseName('')).toBe('');
  });

  it('returns the parent directory and keeps the root stable', () => {
    expect(parentPath('/var/log/syslog')).toBe('/var/log');
    expect(parentPath('/var')).toBe('/');
    expect(parentPath('/')).toBe('/');
    expect(parentPath('')).toBe('/');
  });

  it('formats byte sizes', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1024)).toBe('1 KB');
    expect(formatBytes(1024 * 1024)).toBe('1.0 MB');
  });

  it('quotes paths for the remote shell, escaping embedded single quotes', () => {
    expect(shellQuote('/var/log')).toBe("'/var/log'");
    expect(shellQuote("/srv/it's here")).toBe("'/srv/it'\\''s here'");
  });
});

describe('monitor traffic window totals', () => {
  it('sums counter deltas only inside the visible window', () => {
    const points = [
      { ts: 90, rx: 900, tx: 400 },
      { ts: 100, rx: 1000, tx: 500 },
      { ts: 130, rx: 1100, tx: 550 },
      { ts: 160, rx: 1200, tx: 600 },
      { ts: 170, rx: 1300, tx: 650 },
    ];
    expect(netWindowTotal(points, 100, 160)).toEqual({
      rx: 200,
      tx: 100,
      partial: false,
    });
  });

  it('marks counter resets partial and keeps post-reset traffic', () => {
    const points = [
      { ts: 1000, rx: 1000, tx: 500 },
      { ts: 1030, rx: 100, tx: 50 },
      { ts: 1060, rx: 160, tx: 80 },
    ];
    expect(netWindowTotal(points, 1000, 1060)).toEqual({
      rx: 160,
      tx: 80,
      partial: true,
    });
  });

  it('marks long sampling gaps partial but keeps the counter delta', () => {
    const points = [
      { ts: 1000, rx: 100, tx: 50 },
      { ts: 5000, rx: 1100, tx: 550 },
    ];
    expect(netWindowTotal(points, 1000, 5000)).toEqual({
      rx: 1000,
      tx: 500,
      partial: true,
    });
  });

  it('returns null until two samples are visible', () => {
    expect(netWindowTotal([], 1000, 1060)).toBeNull();
    expect(netWindowTotal([{ ts: 1030, rx: 100, tx: 50 }], 1000, 1060)).toBeNull();
  });
});
