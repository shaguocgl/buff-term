import { useCallback, useEffect, useState } from 'react';
import {
  addMcpRule,
  deleteMcpRule,
  getMcpService,
  listMcpRules,
  rotateMcpToken,
  saveMcpService,
} from '../api';
import type { Host, McpRule, McpService } from '../types';
import Modal from './Modal';
import Select, { type SelectOption } from './Select';
import { CheckIcon, KeyIcon, PowerIcon, ServerIcon } from './Icons';

interface Props {
  hosts: Host[];
  onClose: () => void;
}

type Permission = 'readonly' | 'confirm' | 'allow';

const PERMISSION_OPTIONS: SelectOption<Permission>[] = [
  { value: 'readonly', label: '只读模式（不能进行写操作）' },
  { value: 'confirm', label: '管控模式（预置 + 自定义管控规则）' },
  { value: 'allow', label: '全部放行（可执行任意命令）' },
];

const PERMISSION_HINTS: Record<Permission, string> = {
  readonly:
    '可以执行查看类命令（ps、df、cat 等），写操作（重定向写文件、修改、删除、安装、传输等）会被拒绝。',
  confirm:
    '系统预置的危险命令（rm、mkfs、关机重启等）以及你在 AI 配置中自定义的管控规则命中时，会弹出确认框由你批准。',
  allow: '可执行任意命令，仅记录审计日志。请确保信任外部 AI 的来源。',
};

export default function McpServiceModal({ hosts, onClose }: Props) {
  const [service, setService] = useState<McpService | null>(null);
  const [hostIds, setHostIds] = useState<string[]>([]);
  const [permission, setPermission] = useState<Permission>('confirm');
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [rules, setRules] = useState<McpRule[]>([]);
  const [ruleInput, setRuleInput] = useState('');

  const load = useCallback(async () => {
    const s = await getMcpService();
    setService(s);
    setHostIds(s.host_ids);
    setPermission(s.permission_mode as Permission);
  }, []);

  useEffect(() => {
    load().catch((e) => setError(String(e)));
  }, [load]);

  useEffect(() => {
    listMcpRules()
      .then(setRules)
      .catch((e) => setError(String(e)));
  }, []);

  const toggleHost = (id: string) => {
    setHostIds((prev) =>
      prev.includes(id) ? prev.filter((h) => h !== id) : [...prev, id],
    );
  };

  const apply = async (enabled: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const s = await saveMcpService({
        enabled,
        host_ids: hostIds,
        permission_mode: permission,
      });
      setService(s);
      setCopied(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
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
    try {
      await navigator.clipboard.writeText(configJson(service));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      setError('复制失败，请手动选择复制');
    }
  };

  const handleAddRule = async () => {
    const pattern = ruleInput.trim();
    if (!pattern) return;
    setError(null);
    try {
      const rule = await addMcpRule(pattern);
      setRules((prev) => [rule, ...prev]);
      setRuleInput('');
    } catch (e) {
      setError(String(e));
    }
  };

  const handleDeleteRule = async (id: string) => {
    setError(null);
    try {
      await deleteMcpRule(id);
      setRules((prev) => prev.filter((r) => r.id !== id));
    } catch (e) {
      setError(String(e));
    }
  };

  const running = service?.enabled === true && service.running;
  const dirty =
    service !== null &&
    (hostIds.join(',') !== service.host_ids.join(',') ||
      permission !== service.permission_mode);

  return (
    <Modal
      title="MCP 服务"
      subtitle="KeyWisp 作为 MCP 服务器，把勾选的服务器能力开放给外部 AI（Codex、Claude Desktop 等）"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="mcp-service-modal">
        <div className="mcp-section">
          <div className="mcp-section-title">
            <strong>选择要开放的服务器</strong>
            <span>{hostIds.length} 台已勾选 · list_hosts 只会返回勾选的主机</span>
          </div>
          {hosts.length === 0 ? (
            <div className="mcp-empty">
              <ServerIcon size={26} />
              <p>还没有主机</p>
              <span>请先在左侧新建主机</span>
            </div>
          ) : (
            <div className="mcp-card-list">
              {hosts.map((host) => {
                const checked = hostIds.includes(host.id);
                return (
                  <label
                    key={host.id}
                    className={`mcp-card${checked ? ' checked' : ''}`}
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      onChange={() => toggleHost(host.id)}
                    />
                    <span className="mcp-card-check">
                      {checked && <CheckIcon size={12} />}
                    </span>
                    <span className="mcp-card-avatar">
                      {host.name.slice(0, 1).toUpperCase()}
                    </span>
                    <span className="mcp-card-meta">
                      <span className="mcp-card-name">{host.name}</span>
                      <span className="mcp-card-addr">
                        {host.username}@{host.address}:{host.port}
                      </span>
                    </span>
                    <span className={`tag tag-${host.auth_type}`}>
                      {host.auth_type === 'key' ? '密钥' : '密码'}
                    </span>
                  </label>
                );
              })}
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
          <p className="mcp-hint">{PERMISSION_HINTS[permission]}</p>
        </div>

        {permission === 'confirm' && (
          <div className="mcp-section">
            <div className="mcp-section-title">
              <strong>自定义管控命令</strong>
              <span>命中后执行前需你确认</span>
            </div>
            <div className="rule-add">
              <input
                value={ruleInput}
                onChange={(e) => setRuleInput(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
                    e.preventDefault();
                    handleAddRule().catch(() => {});
                  }
                }}
                placeholder="如 rm -rf，子串匹配，无需通配符"
              />
              <button
                className="btn secondary small"
                onClick={handleAddRule}
                disabled={!ruleInput.trim()}
              >
                添加
              </button>
            </div>
            {rules.length > 0 ? (
              <div className="mcp-rule-list">
                {rules.map((rule) => (
                  <span className="rule-chip" key={rule.id}>
                    <code>{rule.pattern}</code>
                    <button
                      className="rule-del"
                      title="删除"
                      onClick={() => handleDeleteRule(rule.id)}
                    >
                      ×
                    </button>
                  </span>
                ))}
              </div>
            ) : (
              <p className="mcp-hint">
                还没有自定义规则。这里只影响 MCP 服务，与内置 AI 的智能审核规则相互独立。
              </p>
            )}
          </div>
        )}

        {running && service?.token && service.port && (
          <div className="mcp-config-block">
            <div className="mcp-section-title">
              <strong>
                <KeyIcon size={14} /> 外部 AI 接入配置
              </strong>
              <span className="mcp-status">
                <i /> 运行中 · 127.0.0.1:{service.port}
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
              外部 AI 可调用 list_hosts、resource_usage、read_file、list_dir、exec_command。
            </p>
          </div>
        )}

        <div className="mcp-footer">
          {!running && (
            <button
              className="btn primary block"
              onClick={() => apply(true)}
              disabled={busy || hostIds.length === 0}
            >
              <PowerIcon size={14} /> {busy ? '启动中…' : '启动服务'}
            </button>
          )}
          {running && dirty && (
            <button
              className="btn primary block"
              onClick={() => apply(true)}
              disabled={busy || hostIds.length === 0}
            >
              <PowerIcon size={14} /> {busy ? '保存中…' : '保存更改'}
            </button>
          )}
          {running && (
            <button
              className={`btn danger${running && !dirty ? ' block' : ''}`}
              onClick={() => apply(false)}
              disabled={busy}
            >
              <PowerIcon size={14} /> {busy ? '关闭中…' : '关闭服务'}
            </button>
          )}
          {!running && hostIds.length === 0 && hosts.length > 0 && (
            <span className="mcp-footer-hint">请先勾选至少一台服务器</span>
          )}
          {error && <span className="mcp-error">{error}</span>}
        </div>
      </div>
    </Modal>
  );
}

function configJson(service: McpService): string {
  const url = `http://127.0.0.1:${service.port}/mcp`;
  return JSON.stringify(
    {
      mcpServers: {
        'keywisp-ssh': {
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
