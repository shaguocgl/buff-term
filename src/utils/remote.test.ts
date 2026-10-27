import { describe, expect, it } from 'vitest';
import { baseName, formatBytes, joinPath, parentPath, shellQuote } from './remote';

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
