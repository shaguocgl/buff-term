import { useState } from 'react';
import type { FormEvent } from 'react';
import { createHost, saveHostPassword, testHostConnection, updateHost } from '../api';
import type { Host, HostInput, TestResult } from '../types';
import Modal from './Modal';
import { EyeIcon, EyeOffIcon, KeyIcon, ShieldIcon } from './Icons';

interface Props {
  initial?: Host | null;
  onSaved: () => void;
  onCancel: () => void;
}

export default function HostForm({ initial, onSaved, onCancel }: Props) {
  const [name, setName] = useState(initial?.name ?? '');
  const [address, setAddress] = useState(initial?.address ?? '');
  const [port, setPort] = useState(initial?.port.toString() ?? '22');
  const [username, setUsername] = useState(initial?.username ?? '');
  const [authType, setAuthType] = useState<'key' | 'password'>(
    initial?.auth_type ?? 'key',
  );
  const [keyPath, setKeyPath] = useState(initial?.key_path ?? '');
  const [password, setPassword] = useState('');
  const [showPassword, setShowPassword] = useState(false);
  const [notes, setNotes] = useState(initial?.notes ?? '');
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  const handleTest = async () => {
    setError(null);
    setTestResult(null);
    if (!address.trim() || !username.trim()) {
      setError('请先填写地址和用户名');
      return;
    }
    const portNum = Number.parseInt(port, 10);
    if (Number.isNaN(portNum) || portNum < 1 || portNum > 65535) {
      setError('端口无效');
      return;
    }
    const hostLike: Host = {
      id: 'test',
      name: name.trim() || 'test',
      address: address.trim(),
      port: portNum,
      username: username.trim(),
      auth_type: authType,
      key_path: authType === 'key' && keyPath.trim() ? keyPath.trim() : null,
      notes: null,
      created_at: 0,
    };
    setTesting(true);
    try {
      const res = await testHostConnection(
        hostLike,
        authType === 'password' && password ? password : undefined,
      );
      setTestResult(res);
    } catch (err) {
      setTestResult({ ok: false, message: String(err) });
    } finally {
      setTesting(false);
    }
  };

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!name.trim() || !address.trim() || !username.trim()) {
      setError('名称、地址、用户名不能为空');
      return;
    }
    const portNum = Number.parseInt(port, 10);
    if (Number.isNaN(portNum) || portNum < 1 || portNum > 65535) {
      setError('端口无效');
      return;
    }
    const input: HostInput = {
      name: name.trim(),
      address: address.trim(),
      port: portNum,
      username: username.trim(),
      auth_type: authType,
      notes: notes.trim() || undefined,
    };
    if (authType === 'key' && keyPath.trim()) {
      input.key_path = keyPath.trim();
    }

    setSaving(true);
    try {
      if (initial) {
        await updateHost({
          ...initial,
          name: input.name,
          address: input.address,
          port: input.port,
          username: input.username,
          auth_type: input.auth_type,
          key_path: input.key_path ?? null,
          notes: input.notes ?? null,
        });
      } else {
        const host = await createHost(input);
        if (authType === 'password' && password) {
          await saveHostPassword(host.id, password).catch(() => {
            setError('主机已保存，但密码写入系统钥匙串失败');
          });
        }
      }
      if (initial && authType === 'password' && password) {
        await saveHostPassword(initial.id, password).catch(() => {
          setError('主机已保存，但密码写入系统钥匙串失败');
        });
      }
      onSaved();
    } catch (err) {
      setError(String(err));
      setSaving(false);
    }
  };

  return (
    <Modal
      title={initial ? '编辑主机' : '新建主机'}
      subtitle="支持密钥认证与密码认证（密码保存到系统钥匙串）"
      onClose={onCancel}
    >
      <form className="host-form" onSubmit={handleSubmit}>
        <label>
          名称
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="生产服务器"
          />
        </label>

        <div className="form-grid">
          <label>
            地址
            <input
              value={address}
              onChange={(e) => setAddress(e.target.value)}
              placeholder="192.168.1.10"
            />
          </label>
          <label>
            端口
            <input
              value={port}
              onChange={(e) => setPort(e.target.value)}
              inputMode="numeric"
              className="input-port"
            />
          </label>
        </div>

        <label>
          用户名
          <input
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            placeholder="root"
          />
        </label>

        <label>
          认证方式
          <div className="segmented">
            <button
              type="button"
              className={authType === 'key' ? 'seg active' : 'seg'}
              onClick={() => setAuthType('key')}
            >
              <KeyIcon size={14} /> 密钥
            </button>
            <button
              type="button"
              className={authType === 'password' ? 'seg active' : 'seg'}
              onClick={() => setAuthType('password')}
            >
              <ShieldIcon size={14} /> 密码
            </button>
          </div>
        </label>

        {authType === 'key' ? (
          <label>
            私钥路径
            <input
              value={keyPath}
              onChange={(e) => setKeyPath(e.target.value)}
              placeholder="~/.ssh/id_ed25519（留空使用默认）"
            />
          </label>
        ) : (
          <label>
            密码
            <div className="password-input-wrap">
              <input
                type={showPassword ? 'text' : 'password'}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder={
                  initial
                    ? '留空保持原密码不变；若从未保存过，留空则连接时需手动输入'
                    : '留空则连接时手动输入'
                }
              />
              <button
                type="button"
                className="password-eye"
                title={showPassword ? '隐藏密码' : '显示密码'}
                onClick={() => setShowPassword((v) => !v)}
              >
                {showPassword ? <EyeOffIcon size={16} /> : <EyeIcon size={16} />}
              </button>
            </div>
          </label>
        )}

        <label>
          备注
          <input
            value={notes}
            onChange={(e) => setNotes(e.target.value)}
            placeholder="可选"
          />
        </label>

        {error && <p className="error">{error}</p>}
        {testResult && (
          <div className={`test-result ${testResult.ok ? 'ok' : 'err'}`}>
            {testResult.ok ? '✓' : '!'} {testResult.message}
          </div>
        )}

        <div className="form-actions">
          <button
            type="button"
            className="btn secondary"
            onClick={handleTest}
            disabled={testing || saving}
          >
            {testing ? '测试中…' : '测试连接'}
          </button>
          <button type="button" className="btn ghost" onClick={onCancel}>
            取消
          </button>
          <button type="submit" className="btn primary" disabled={saving}>
            {saving ? '保存中…' : '保存'}
          </button>
        </div>
      </form>
    </Modal>
  );
}
