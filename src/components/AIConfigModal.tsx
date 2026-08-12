import { useCallback, useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import {
  addAiRule,
  deleteAiRule,
  deleteAiProvider,
  listAiProviders,
  listAiRules,
  saveAiProvider,
  testAiProvider,
} from '../api';
import type { AiModelInput, AiProvider, AiRule, TestResult } from '../types';
import Modal from './Modal';
import { CheckIcon, PlusIcon, SparklesIcon, TrashIcon } from './Icons';
import Select from './Select';

interface Props {
  onClose: () => void;
  onSaved: () => void;
}

interface Preset {
  name: string;
  base_url: string;
  models: { label: string; model: string }[];
}

const PRESETS: Preset[] = [
  {
    name: 'DeepSeek',
    base_url: 'https://api.deepseek.com',
    models: [
      { label: 'DeepSeek V4 Flash', model: 'deepseek-v4-flash' },
      { label: 'DeepSeek V4 Pro', model: 'deepseek-v4-pro' },
    ],
  },
  {
    name: 'OpenAI',
    base_url: 'https://api.openai.com/v1',
    models: [
      { label: 'GPT-4o mini', model: 'gpt-4o-mini' },
      { label: 'GPT-4o', model: 'gpt-4o' },
    ],
  },
  {
    name: '通义千问',
    base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
    models: [
      { label: 'Qwen Plus', model: 'qwen-plus' },
      { label: 'Qwen Max', model: 'qwen-max' },
    ],
  },
  {
    name: 'Kimi',
    base_url: 'https://api.moonshot.cn/v1',
    models: [
      { label: 'Moonshot 8K', model: 'moonshot-v1-8k' },
      { label: 'Moonshot 32K', model: 'moonshot-v1-32k' },
    ],
  },
  {
    name: 'Ollama（本地）',
    base_url: 'http://localhost:11434/v1',
    models: [
      { label: 'Qwen2.5 7B', model: 'qwen2.5:7b' },
      { label: 'Llama3.1 8B', model: 'llama3.1:8b' },
    ],
  },
];

interface FormModel {
  id?: string;
  label: string;
  model: string;
  is_active: boolean;
}

interface FormState {
  preset: string;
  name: string;
  base_url: string;
  models: FormModel[];
  apiKey: string;
}

function presetModels(preset: Preset): FormModel[] {
  return preset.models.map((m, idx) => ({
    label: m.label,
    model: m.model,
    is_active: idx === 0,
  }));
}

function formFromPreset(preset: Preset): FormState {
  return {
    preset: preset.name,
    name: preset.name,
    base_url: preset.base_url,
    models: presetModels(preset),
    apiKey: '',
  };
}

export default function AIConfigModal({ onClose, onSaved }: Props) {
  const [providers, setProviders] = useState<AiProvider[]>([]);
  const [rules, setRules] = useState<AiRule[]>([]);
  const [ruleInput, setRuleInput] = useState('');
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<AiProvider | null>(null);
  const [form, setForm] = useState<FormState>(() => formFromPreset(PRESETS[0]));
  const [enabled, setEnabled] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  const load = useCallback(async () => {
    setProviders(await listAiProviders());
  }, []);

  useEffect(() => {
    load().catch((e) => setError(String(e)));
    listAiRules()
      .then(setRules)
      .catch(() => {});
  }, [load]);

  const handleAddRule = async () => {
    const pattern = ruleInput.trim();
    if (!pattern) return;
    try {
      const rule = await addAiRule(pattern);
      setRules((prev) => [rule, ...prev]);
      setRuleInput('');
    } catch (err) {
      setError(String(err));
    }
  };

  const handleDeleteRule = async (id: string) => {
    try {
      await deleteAiRule(id);
      setRules((prev) => prev.filter((r) => r.id !== id));
    } catch (err) {
      setError(String(err));
    }
  };

  const openCreate = () => {
    setEditing(null);
    setForm(formFromPreset(PRESETS[0]));
    setEnabled(true);
    setTestResult(null);
    setError(null);
    setShowForm(true);
  };

  const openEdit = (p: AiProvider) => {
    setEditing(p);
    setForm({
      preset: '自定义',
      name: p.name,
      base_url: p.base_url,
      models: p.models.map((m) => ({
        id: m.id,
        label: m.label,
        model: m.model,
        is_active: m.is_active,
      })),
      apiKey: '',
    });
    setEnabled(p.enabled);
    setTestResult(null);
    setError(null);
    setShowForm(true);
  };

  const applyPreset = (name: string) => {
    if (name === '自定义') {
      // 清空下方所有字段，方便从零配置
      setForm({
        preset: '自定义',
        name: '',
        base_url: '',
        models: [],
        apiKey: '',
      });
      return;
    }
    const preset = PRESETS.find((p) => p.name === name);
    if (!preset) return;
    setForm((prev) => ({
      ...formFromPreset(preset),
      apiKey: prev.apiKey,
    }));
  };

  const updateModel = (idx: number, patch: Partial<FormModel>) => {
    setForm((prev) => ({
      ...prev,
      models: prev.models.map((m, i) => (i === idx ? { ...m, ...patch } : m)),
    }));
  };

  const setActiveModel = (idx: number) => {
    setForm((prev) => ({
      ...prev,
      models: prev.models.map((m, i) => ({ ...m, is_active: i === idx })),
    }));
  };

  const addModel = () => {
    setForm((prev) => ({
      ...prev,
      models: [
        ...prev.models,
        {
          label: '',
          model: '',
          is_active: prev.models.length === 0,
        },
      ],
    }));
  };

  const removeModel = (idx: number) => {
    setForm((prev) => {
      const models = prev.models.filter((_, i) => i !== idx);
      if (models.length > 0 && !models.some((m) => m.is_active)) {
        models[0].is_active = true;
      }
      return { ...prev, models };
    });
  };

  const activeModel = () =>
    form.models.find((m) => m.is_active) ?? form.models[0];

  const handleSave = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    const models: AiModelInput[] = form.models
      .filter((m) => m.label.trim() && m.model.trim())
      .map((m, idx) => ({
        label: m.label.trim(),
        model: m.model.trim(),
        is_active:
          m.is_active ||
          (form.models.filter((x) => x.label.trim() && x.model.trim()).length === 1) ||
          (idx === 0 && !form.models.some((x) => x.is_active)),
      }));
    if (!form.name.trim() || !form.base_url.trim()) {
      setError('名称、Base URL 不能为空');
      return;
    }
    if (models.length === 0) {
      setError('至少需要配置一个模型');
      return;
    }
    setSaving(true);
    try {
      await saveAiProvider(
        {
          name: form.name.trim(),
          base_url: form.base_url.trim(),
          enabled,
          models,
          ...(form.apiKey.trim() ? { api_key: form.apiKey.trim() } : {}),
        },
        editing?.id,
      );
      setShowForm(false);
      await load();
      onSaved();
    } catch (err) {
      setError(String(err));
    } finally {
      setSaving(false);
    }
  };

  const handleTest = async () => {
    const model = activeModel();
    if (!form.base_url.trim() || !model) {
      setError('请先填写 Base URL 并至少添加一个模型');
      return;
    }
    setTesting(true);
    setTestResult(null);
    try {
      const result = await testAiProvider({
        base_url: form.base_url.trim(),
        model: model.model.trim(),
        api_key: form.apiKey.trim() || undefined,
        id: editing?.id,
      });
      setTestResult(result);
    } catch (err) {
      setTestResult({ ok: false, message: String(err) });
    } finally {
      setTesting(false);
    }
  };

  const handleDelete = async (p: AiProvider) => {
    if (!window.confirm(`删除 AI 配置 "${p.name}"？`)) return;
    try {
      await deleteAiProvider(p.id);
      await load();
      onSaved();
    } catch (err) {
      setError(String(err));
    }
  };

  const handleEnable = async (p: AiProvider) => {
    try {
      await saveAiProvider(
        {
          name: p.name,
          base_url: p.base_url,
          protocol: p.protocol,
          enabled: true,
          models: p.models.map((m) => ({
            label: m.label,
            model: m.model,
            is_active: m.is_active,
          })),
        },
        p.id,
      );
      await load();
      onSaved();
    } catch (err) {
      setError(String(err));
    }
  };

  return (
    <Modal
      title="AI 配置"
      subtitle="配置大模型平台与多个模型，供 AI Agent 管理服务器使用"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal">
        {!showForm ? (
          <>
            <div className="ai-modal-actions">
              <button className="btn primary block" onClick={openCreate}>
                <PlusIcon size={16} /> 新建配置
              </button>
            </div>

            {providers.length === 0 ? (
              <div className="ai-empty">
                <SparklesIcon size={30} />
                <p>还没有配置模型平台</p>
                <span>选择 DeepSeek / OpenAI / 通义 / Kimi / 本地 Ollama</span>
              </div>
            ) : (
              <div className="provider-list">
                {providers.map((p) => (
                  <div key={p.id} className="provider-card">
                    <div className="provider-meta">
                      <div className="provider-name-row">
                        <span className="provider-name">{p.name}</span>
                        {p.enabled ? (
                          <span className="badge badge-on">
                            <CheckIcon size={11} /> 已启用
                          </span>
                        ) : (
                          <span className="badge badge-off">未启用</span>
                        )}
                      </div>
                      <span className="provider-detail">
                        {p.models.length} 个模型 · {p.base_url}
                      </span>
                      <div className="provider-model-chips">
                        {p.models.map((m) => (
                          <span
                            key={m.id}
                            className={`provider-model-chip${m.is_active ? ' active' : ''}`}
                            title={m.is_active ? '默认模型' : m.model}
                          >
                            {m.label}
                          </span>
                        ))}
                      </div>
                    </div>
                    <div className="provider-actions">
                      {!p.enabled && (
                        <button className="btn ghost small" onClick={() => handleEnable(p)}>
                          启用
                        </button>
                      )}
                      <button className="btn ghost small" onClick={() => openEdit(p)}>
                        编辑
                      </button>
                      <button
                        className="icon-btn danger"
                        title="删除配置"
                        onClick={() => handleDelete(p)}
                      >
                        <TrashIcon size={15} />
                      </button>
                    </div>
                  </div>
                ))}
              </div>
            )}

            <div className="rules-section">
              <div className="rules-header">
                <span className="rules-title">智能审核规则</span>
                <span className="rules-hint">
                  智能审核模式下，命令命中这些模式即需要你批准（不区分大小写）
                </span>
              </div>
              <div className="rules-note">
                匹配方式：<strong>子串匹配</strong>，命令文本包含该片段即命中，不区分大小写，
                无需通配符。例如配置 <code>rm -rf</code>，可命中{' '}
                <code>rm -rf /data</code>、<code>rm -rf /var/log/*</code> 等所有包含
                “rm -rf”的命令。请直接填写最小片段，不要使用 <code>*</code> 或正则。
              </div>
              <div className="rules-list">
                {rules.length === 0 && (
                  <span className="rules-empty">暂无自定义规则</span>
                )}
                {rules.map((r) => (
                  <div className="rule-chip" key={r.id}>
                    <code>{r.pattern}</code>
                    <button
                      className="rule-del"
                      title="删除规则"
                      onClick={() => handleDeleteRule(r.id)}
                    >
                      ×
                    </button>
                  </div>
                ))}
              </div>
              <div className="rule-add">
                <input
                  value={ruleInput}
                  onChange={(e) => setRuleInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      handleAddRule();
                    }
                  }}
                  placeholder="如：git push --force（无需通配符）"
                />
                <button className="btn secondary small" onClick={handleAddRule}>
                  添加
                </button>
              </div>
            </div>
          </>
        ) : (
          <form className="ai-form" onSubmit={handleSave}>
            <div className="ai-field">
              <span className="ai-field-label">平台预设</span>
              <Select
                value={form.preset}
                options={[
                  ...PRESETS.map((p) => ({ value: p.name, label: p.name })),
                  { value: '自定义', label: '自定义' },
                ]}
                onChange={applyPreset}
                ariaLabel="平台预设"
              />
            </div>
            <label>
              名称
              <input
                value={form.name}
                onChange={(e) => setForm({ ...form, name: e.target.value })}
                placeholder="我的 DeepSeek"
              />
            </label>
            <label>
              Base URL
              <input
                value={form.base_url}
                onChange={(e) => setForm({ ...form, base_url: e.target.value })}
                placeholder="https://api.deepseek.com"
              />
            </label>
            <label>
              API Key
              <input
                type="password"
                value={form.apiKey}
                onChange={(e) => setForm({ ...form, apiKey: e.target.value })}
                placeholder={
                  editing
                    ? '已保存，留空保持不变（存系统钥匙串）'
                    : '保存到系统钥匙串'
                }
              />
            </label>

            <div className="model-editor">
              <span className="model-editor-label">模型列表</span>
              {form.models.map((m, idx) => (
                <div className="model-row" key={idx}>
                  <input
                    value={m.label}
                    placeholder="显示名称，如 DeepSeek V4 Flash"
                    onChange={(e) => updateModel(idx, { label: e.target.value })}
                  />
                  <input
                    value={m.model}
                    placeholder="模型 ID，如 deepseek-v4-flash"
                    className="model-id-input"
                    onChange={(e) => updateModel(idx, { model: e.target.value })}
                  />
                  <button
                    type="button"
                    className={`model-default${m.is_active ? ' on' : ''}`}
                    title="设为默认模型"
                    onClick={() => setActiveModel(idx)}
                  >
                    {m.is_active ? '默认' : '设默认'}
                  </button>
                  <button
                    type="button"
                    className="icon-btn danger"
                    title="删除模型"
                    onClick={() => removeModel(idx)}
                  >
                    <TrashIcon size={14} />
                  </button>
                </div>
              ))}
              <button type="button" className="btn secondary small add-model" onClick={addModel}>
                <PlusIcon size={13} /> 添加模型
              </button>
            </div>

            <div className="form-row-inline">
              <span className="form-label">设为当前使用的模型平台</span>
              <button
                type="button"
                className={`switch${enabled ? ' on' : ''}`}
                onClick={() => setEnabled(!enabled)}
                aria-label="启用开关"
              >
                <span />
              </button>
            </div>

            {testResult && (
              <div className={`test-result ${testResult.ok ? 'ok' : 'err'}`}>
                {testResult.ok ? '✓' : '!'} {testResult.message}
              </div>
            )}
            {error && <p className="error">{error}</p>}

            <div className="form-actions">
              <button
                type="button"
                className="btn ghost"
                onClick={() => setShowForm(false)}
              >
                取消
              </button>
              <button
                type="button"
                className="btn secondary"
                onClick={handleTest}
                disabled={testing}
              >
                {testing ? '测试中…' : '测试连接'}
              </button>
              <button type="submit" className="btn primary" disabled={saving}>
                {saving ? '保存中…' : '保存'}
              </button>
            </div>
          </form>
        )}
      </div>
    </Modal>
  );
}
