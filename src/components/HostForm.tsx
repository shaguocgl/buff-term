import { useState } from 'react';
import type { FormEvent } from 'react';
import { createHost, deleteHost, saveHostKeyPassphrase, saveHostPassword, testHostConnection, updateHost } from '../api';
import { useT } from '../i18n';
import type { Host, HostInput, TestResult } from '../types';
import { fmtError } from '../utils/errors';
import Modal from './Modal';
import { EyeIcon, EyeOffIcon, KeyIcon, ShieldIcon } from './Icons';

interface Props {
  initial?: Host | null;
  onSaved: () => void;
  onCancel: () => void;
}

export default function HostForm({ initial, onSaved, onCancel }: Props) {
  const t = useT();
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
  const [passphrase, setPassphrase] = useState('');
  const [showPassphrase, setShowPassphrase] = useState(false);
  const [notes, setNotes] = useState(initial?.notes ?? '');
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  const handleTest = async () => {
    setError(null);
    setTestResult(null);
    if (!address.trim() || !username.trim()) {
      setError(t('hostForm.needAddressUser'));
      return;
    }
    const portNum = Number.parseInt(port, 10);
    if (Number.isNaN(portNum) || portNum < 1 || portNum > 65535) {
      setError(t('hostForm.badPort'));
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
        authType === 'key' && passphrase ? passphrase : undefined,
      );
      setTestResult(res);
    } catch (err) {
      setTestResult({ ok: false, message: fmtError(err) });
    } finally {
      setTesting(false);
    }
  };

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!name.trim() || !address.trim() || !username.trim()) {
      setError(t('hostForm.needFields'));
      return;
    }
    const portNum = Number.parseInt(port, 10);
    if (Number.isNaN(portNum) || portNum < 1 || portNum > 65535) {
      setError(t('hostForm.badPort'));
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
          // 密码写入失败必须中止保存流程：回滚刚创建的主机，
          // 避免用户重试时重复创建记录
          try {
            await saveHostPassword(host.id, password);
          } catch (e) {
            await deleteHost(host.id).catch(() => {});
            setError(
              t('hostForm.passwordSaveFailedNew', { err: fmtError(e) }),
            );
            setSaving(false);
            return;
          }
        }
        if (authType === 'key' && passphrase) {
          try {
            await saveHostKeyPassphrase(host.id, passphrase);
          } catch (e) {
            await deleteHost(host.id).catch(() => {});
            setError(
              t('hostForm.passphraseSaveFailedNew', { err: fmtError(e) }),
            );
            setSaving(false);
            return;
          }
        }
      }
      if (initial && authType === 'password' && password) {
        try {
          await saveHostPassword(initial.id, password);
        } catch (e) {
          // 编辑场景：主机信息已更新（幂等），仅密码未保存，停留表单便于重试
          setError(
            t('hostForm.passwordSaveFailedEdit', { err: fmtError(e) }),
          );
          setSaving(false);
          return;
        }
      }
      if (initial && authType === 'key' && passphrase) {
        try {
          await saveHostKeyPassphrase(initial.id, passphrase);
        } catch (e) {
          setError(
            t('hostForm.passphraseSaveFailedEdit', { err: fmtError(e) }),
          );
          setSaving(false);
          return;
        }
      }
      onSaved();
    } catch (err) {
      setError(fmtError(err));
      setSaving(false);
    }
  };

  return (
    <Modal
      title={initial ? t('hostForm.titleEdit') : t('hostForm.titleNew')}
      subtitle={t('hostForm.subtitle')}
      onClose={onCancel}
    >
      <form className="host-form" onSubmit={handleSubmit}>
        <label>
          {t('hostForm.name')}
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder={t('hostForm.namePlaceholder')}
          />
        </label>

        <div className="form-grid">
          <label>
            {t('hostForm.address')}
            <input
              value={address}
              onChange={(e) => setAddress(e.target.value)}
              placeholder="192.168.1.10"
            />
          </label>
          <label>
            {t('hostForm.port')}
            <input
              value={port}
              onChange={(e) => setPort(e.target.value)}
              inputMode="numeric"
              className="input-port"
            />
          </label>
        </div>

        <label>
          {t('hostForm.username')}
          <input
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            placeholder="root"
          />
        </label>

        <label>
          {t('hostForm.auth')}
          <div className="segmented">
            <button
              type="button"
              className={authType === 'key' ? 'seg active' : 'seg'}
              onClick={() => setAuthType('key')}
            >
              <KeyIcon size={14} /> {t('hostForm.authKey')}
            </button>
            <button
              type="button"
              className={authType === 'password' ? 'seg active' : 'seg'}
              onClick={() => setAuthType('password')}
            >
              <ShieldIcon size={14} /> {t('hostForm.authPassword')}
            </button>
          </div>
        </label>

        {authType === 'key' ? (
          <>
            <label>
              {t('hostForm.keyPath')}
              <input
                value={keyPath}
                onChange={(e) => setKeyPath(e.target.value)}
                placeholder={t('hostForm.keyPathPlaceholder')}
              />
            </label>
            <label>
              {t('hostForm.passphrase')}
              <div className="password-input-wrap">
                <input
                  type={showPassphrase ? 'text' : 'password'}
                  value={passphrase}
                  onChange={(e) => setPassphrase(e.target.value)}
                  placeholder={
                    initial
                      ? t('hostForm.passphrasePlaceholderEdit')
                      : t('hostForm.passphrasePlaceholderNew')
                  }
                />
                <button
                  type="button"
                  className="password-eye"
                  title={
                    showPassphrase
                      ? t('hostForm.hidePassphrase')
                      : t('hostForm.showPassphrase')
                  }
                  onClick={() => setShowPassphrase((v) => !v)}
                >
                  {showPassphrase ? <EyeOffIcon size={16} /> : <EyeIcon size={16} />}
                </button>
              </div>
            </label>
          </>
        ) : (
          <label>
            {t('hostForm.authPassword')}
            <div className="password-input-wrap">
              <input
                type={showPassword ? 'text' : 'password'}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder={
                  initial
                    ? t('hostForm.passwordPlaceholderEdit')
                    : t('hostForm.passwordPlaceholderNew')
                }
              />
              <button
                type="button"
                className="password-eye"
                title={
                  showPassword
                    ? t('hostForm.hidePassword')
                    : t('hostForm.showPassword')
                }
                onClick={() => setShowPassword((v) => !v)}
              >
                {showPassword ? <EyeOffIcon size={16} /> : <EyeIcon size={16} />}
              </button>
            </div>
          </label>
        )}

        <label>
          {t('hostForm.notes')}
          <input
            value={notes}
            onChange={(e) => setNotes(e.target.value)}
            placeholder={t('hostForm.notesPlaceholder')}
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
            {testing ? t('common.testing') : t('hostForm.test')}
          </button>
          <button type="button" className="btn ghost" onClick={onCancel}>
            {t('common.cancel')}
          </button>
          <button type="submit" className="btn primary" disabled={saving}>
            {saving ? t('common.saving') : t('common.save')}
          </button>
        </div>
      </form>
    </Modal>
  );
}
