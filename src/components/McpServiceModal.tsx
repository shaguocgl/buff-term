import { useCallback, useEffect, useState } from 'react';
import { getMcpService, rotateMcpToken, saveMcpService } from '../api';
import type { Host, McpService } from '../types';
import Modal from './Modal';
import Select, { type SelectOption } from './Select';
import { CheckIcon } from './Icons';

interface Props {
  hosts: Host[];
  onClose: () => void;
}

type Permission = 'readonly' | 'confirm' | 'allow';

const PERMISSION_OPTIONS: SelectOption<Permission>[] = [
  { value: 'readonly', label: '只读（不能执行命令）' },
  { value: 'confirm', label: '危险命令需确认' },
  { value: 'allow', label: '全部放行' },
];

export default function McpServiceModal({ hosts, onClose }: Props) {
  const [service, setService] = useState<McpService | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [hostIds, setHostIds] = useState<string[]>([]);
  const [permission, setPermission] = useState<Permission>('confirm');
  const [saving, setSaving] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    const s = await getMcpService();
    setService(s);
    setEnabled(s.enabled);
    setHostIds(s.host_ids);
    setPermission(s.permission_mode as Permission);
  }, []);

  useEffect(() => {
    load().catch((e) => setError(String(e)));
  }, [load]);

  const toggleHost = (id: string) => {
    setHostIds((prev) =>
      prev.includes(id) ? prev.filter((h) => h !== id) : [...prev, id],
    );
  };

  const handleSave = async () => {
    setSaving(true);
    setError(null);
    try {
      const s = await saveMcpService({
        enabled,
        host_ids: hostIds,
        permission_mode: permission,
      });
      setService(s);
      setEnabled(s.enabled);
      setHostIds(s.host_ids);
      setPermission(s.permission_mode as Permission);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const handleRotate = async () => {
    setError(null);
    try {
      const s = await rotateMcpToken();
      setService(s);
      setCopied(false);
    } catch (e) {
      setError(String(e));
    }
  };

  const copyConfig = async () => {
    if (!service?.token || !service?.port) return;
    const json = configJson(service);
    try {
      await navigator.clipboard.writeText(json);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      setError('复制失败，请手动选择复制');
    }
  };

  const running = service?.running && service.enabled;

  return (
    <Modal
      title="MCP 服务"
      subtitle="KeyWisp 作为 MCP 服务器，把勾选的服务器能力开放给外部 AI（Codex、Claude Desktop 等）"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="mcp-service-modal">
        <div className="mcp-switch-row">
          <div className="mcp-switch-text">
            <strong>启用 MCP 服务</strong>
            <span>启用后才开始监听本机端口，平时不占用资源</span>
          </div>
          <button
            type="button"
            className={`switch${enabled ? ' on' : ''}`}
            onClick={() => setEnabled((v) => !v)}
            aria-label="启用 MCP 服务"
          >
            <span />
          </button>
        </div>

        {enabled && (
          <>
            <div className="mcp-section">
              <div className="mcp-section-title">
                <strong>允许外部 AI 操作的服务器</strong>
                <span>list_hosts 只会返回这里勾选的主机</span>
              </div>
              {hosts.length === 0 ? (
                <p className="mcp-empty">还没有主机，请先在左侧新建主机</p>
              ) : (
                <div className="mcp-host-list">
                  {hosts.map((host) => (
                    <label key={host.id} className="mcp-host-item">
                      <input
                        type="checkbox"
                        checked={hostIds.includes(host.id)}
                        onChange={() => toggleHost(host.id)}
                      />
                      <span className="mcp-host-name">{host.name}</span>
                      <span className="mcp-host-addr">
                        {host.username}@{host.address}:{host.port}
                      </span>
                    </label>
                  ))}
                </div>
              )}
            </div>

            <div className="mcp-section">
              <div className="mcp-section-title">
                <strong>权限模式</strong>
                <span>外部 AI 执行命令时的审核策略</span>
              </div>
              <Select
                value={permission}
                options={PERMISSION_OPTIONS}
                onChange={setPermission}
                ariaLabel="权限模式"
              />
              <p className="mcp-hint">
                {permission === 'readonly' && '只能查看资源占用、读取文件和列目录，不能执行任何命令。'}
                {permission === 'confirm' && '危险命令（rm -rf、mkfs、关机重启等）会弹出确认框，由你在 KeyWisp 中批准。'}
                {permission === 'allow' && '全部放行，仅记录审计日志。请确保信任外部 AI 的来源。'}
              </p>
            </div>

            <div className="mcp-actions">
              <button
                className="btn primary"
                onClick={handleSave}
                disabled={saving || hosts.length === 0 || (enabled && hostIds.length === 0)}
              >
                {saving ? '保存中…' : running ? '保存配置' : '保存并启动'}
              </button>
              {enabled && hostIds.length === 0 && (
                <span className="mcp-error">请至少勾选一台服务器</span>
              )}
              {error && <span className="mcp-error">{error}</span>}
            </div>

            {running && service?.token && service.port && (
              <div className="mcp-config-block">
                <div className="mcp-section-title">
                  <strong>外部 AI 接入配置</strong>
                  <span>
                    服务运行中 · http://127.0.0.1:{service.port}/mcp
                  </span>
                </div>
                <pre className="mcp-config-json">{configJson(service)}</pre>
                <div className="mcp-config-actions">
                  <button className="btn secondary small" onClick={copyConfig}>
                    {copied ? (
                      <>
                        <CheckIcon size={14} /> 已复制
                      </>
                    ) : (
                      '复制配置'
                    )}
                  </button>
                  <button className="btn ghost small" onClick={handleRotate}>
                    吊销并重新生成 token
                  </button>
                </div>
                <p className="mcp-hint">
                  在 Codex / Claude Desktop 的 MCP 配置中粘贴上面 JSON 即可接入。
                  外部 AI 可调用 list_hosts、resource_usage、read_file、list_dir
                  {permission !== 'readonly' && '、exec_command'}。
                </p>
              </div>
            )}
          </>
        )}
      </div>
    </Modal>
  );
}

function configJson(service: McpService): string {
  const url = `http://127.0.0.1:${service.port}/mcp`;
  return JSON.stringify(
    {
      mcpServers: {
        keywisp: {
          type: 'http',
          url,
          headers: { Authorization: `Bearer ${service.token}` },
        },
      },
    },
    null,
    2,
  );
}
