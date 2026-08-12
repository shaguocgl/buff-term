import { useCallback, useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import {
  deleteMcpServer,
  listMcpServers,
  mcpTest,
  saveMcpServer,
} from '../api';
import type { McpServer, McpToolInfo } from '../types';
import Modal from './Modal';
import { PlusIcon, TrashIcon, WrenchIcon } from './Icons';

interface Props {
  onClose: () => void;
}

export default function McpModal({ onClose }: Props) {
  const [servers, setServers] = useState<McpServer[]>([]);
  const [name, setName] = useState('');
  const [command, setCommand] = useState('');
  const [args, setArgs] = useState('');
  const [testingId, setTestingId] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<Record<string, McpToolInfo[] | string>>({});
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setServers(await listMcpServers());
  }, []);

  useEffect(() => {
    load().catch((e) => setError(String(e)));
  }, [load]);

  const handleAdd = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!name.trim() || !command.trim()) {
      setError('名称与启动命令不能为空');
      return;
    }
    try {
      await saveMcpServer({
        name: name.trim(),
        command: command.trim(),
        args: args.trim(),
        enabled: true,
      });
      setName('');
      setCommand('');
      setArgs('');
      await load();
    } catch (err) {
      setError(String(err));
    }
  };

  const handleDelete = async (server: McpServer) => {
    if (!window.confirm(`删除 MCP 服务器 "${server.name}"？`)) return;
    try {
      await deleteMcpServer(server.id);
      await load();
    } catch (err) {
      setError(String(err));
    }
  };

  const handleTest = async (server: McpServer) => {
    setTestingId(server.id);
    setError(null);
    try {
      const tools = await mcpTest(server);
      setTestResult((prev) => ({ ...prev, [server.id]: tools }));
    } catch (err) {
      setTestResult((prev) => ({ ...prev, [server.id]: String(err) }));
    } finally {
      setTestingId(null);
    }
  };

  return (
    <Modal
      title="MCP 工具"
      subtitle="配置外部 MCP 服务器，AI Agent 可通过 use_mcp_tool 调用其工具"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal">
        <form className="alert-form" onSubmit={handleAdd}>
          <div className="alert-form-row">
            <label>
              名称
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="如 filesystem"
              />
            </label>
            <label>
              启动命令
              <input
                value={command}
                onChange={(e) => setCommand(e.target.value)}
                placeholder="如 npx"
              />
            </label>
          </div>
          <div className="alert-form-row">
            <label className="alert-target">
              命令参数（空格分隔）
              <input
                value={args}
                onChange={(e) => setArgs(e.target.value)}
                placeholder="如 -y @modelcontextprotocol/server-filesystem /Users/mac/Desktop"
              />
            </label>
            <button type="submit" className="btn primary">
              <PlusIcon size={14} /> 添加
            </button>
          </div>
          {error && <p className="error">{error}</p>}
        </form>

        <div className="mcp-list">
          {servers.length === 0 && (
            <div className="ai-empty">
              <WrenchIcon size={26} />
              <p>暂无 MCP 服务器</p>
              <span>添加后 AI 可在对话中调用其工具（需要支持 stdio 传输）</span>
            </div>
          )}
          {servers.map((server) => {
            const result = testResult[server.id];
            return (
              <div key={server.id} className="mcp-item">
                <div className="mcp-head">
                  <span className="mcp-name">{server.name}</span>
                  <span className={`badge ${server.enabled ? 'badge-on' : 'badge-off'}`}>
                    {server.enabled ? '已启用' : '已停用'}
                  </span>
                  <button
                    className="btn ghost small"
                    disabled={testingId === server.id}
                    onClick={() => handleTest(server)}
                  >
                    {testingId === server.id ? '测试中…' : '测试连接'}
                  </button>
                  <button
                    className="icon-btn danger"
                    title="删除"
                    onClick={() => handleDelete(server)}
                  >
                    <TrashIcon size={14} />
                  </button>
                </div>
                <code className="mcp-command">
                  {server.command} {server.args}
                </code>
                {result && (
                  <div className="mcp-result">
                    {Array.isArray(result) ? (
                      result.length > 0 ? (
                        <div className="mcp-tools">
                          {result.map((t) => (
                            <div key={t.name} className="mcp-tool">
                              <span className="mcp-tool-name">{t.name}</span>
                              <span className="mcp-tool-desc">{t.description}</span>
                            </div>
                          ))}
                        </div>
                      ) : (
                        <span className="mcp-tools-empty">连接成功，但服务器未暴露工具</span>
                      )
                    ) : (
                      <span className="mcp-tools-empty err">{result}</span>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      </div>
    </Modal>
  );
}
