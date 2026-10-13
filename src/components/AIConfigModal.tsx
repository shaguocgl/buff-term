import { useCallback, useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import {
  addAiRule,
  deleteAiRule,
  deleteAiProvider,
  getAiMaxToolRounds,
  listAiProviders,
  listAiRules,
  listRemoteAiModels,
  saveAiMaxToolRounds,
  saveAiProvider,
  testAiProvider,
} from '../api';
import { useT, type DictKey } from '../i18n';
import type {
  AiModelInput,
  AiProvider,
  AiRule,
  RemoteAiModel,
  TestResult,
} from '../types';
import { fmtError } from '../utils/errors';
import ConfirmModal from './ConfirmModal';
import Modal from './Modal';
import { CheckIcon, DownloadIcon, PlusIcon, SparklesIcon, TrashIcon } from './Icons';
import Select from './Select';
import type { ToastItem } from './Toast';

interface Props {
  onClose: () => void;
  onSaved: () => void;
  /** 全局提示：保存成功/失败后给出反馈 */
  showToast: (kind: ToastItem['kind'], message: string) => void;
}

interface Preset {
  /** 预设标识（内部值，用于选中与匹配 base_url，不随界面语言变化） */
  name: string;
  base_url: string;
  /** 界面展示名（缺省时用 name，如 DeepSeek / OpenAI 等品牌名） */
  labelKey?: DictKey;
}

/** 「自定义」预设的内部标识 */
const CUSTOM_PRESET = '__custom__';

const PRESETS: Preset[] = [
  { name: 'DeepSeek', base_url: 'https://api.deepseek.com' },
  { name: 'OpenAI', base_url: 'https://api.openai.com/v1' },
  {
    name: '通义千问',
    base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
    labelKey: 'ai.presetQwen',
  },
  { name: 'Kimi', base_url: 'https://api.moonshot.cn/v1' },
  {
    name: 'Ollama（本地）',
    base_url: 'http://localhost:11434/v1',
    labelKey: 'ai.presetOllama',
  },
];

/** 预设的界面展示名 */
const presetLabel = (p: Preset, t: (key: DictKey) => string) =>
  p.labelKey ? t(p.labelKey) : p.name;

interface FormModel {
  id?: string;
  label: string;
  model: string;
  is_active: boolean;
  context_window: number;
  /** 该模型是否支持图片输入（多模态） */
  supports_vision: boolean;
}

/** 新增/导入模型时预填的上下文窗口；保存后仍可在模型行单独修改 */
const DEFAULT_CONTEXT_WINDOW = 128_000;

/** 按模型 ID 粗判是否可能是视觉模型，用于拉取导入时预填勾选；用户可随时改 */
function guessVisionModel(modelId: string): boolean {
  const id = modelId.toLowerCase();
  return /vl|vision|4o|4\.1|4\.5|gemini|claude|glm-4v|pixtral|llava|o3|o4|grok-4|qwen-vl|doubao-vision|step-.*v|hunyuan-vision|minicpm-v|internvl|ernie-.*v|kimi-latest|sensechat-vision|abab.*v/.test(
    id,
  );
}

/** 编辑已有配置时按 base_url 反查平台预设：
 *  归一化尾部「/」，并容忍保存值比预设多一个 /v1 后缀（如 DeepSeek） */
function presetOf(baseUrl: string): string {
  const norm = (u: string) => u.trim().replace(/\/+$/, '');
  const target = norm(baseUrl);
  const hit = PRESETS.find(
    (p) => norm(p.base_url) === target || `${norm(p.base_url)}/v1` === target,
  );
  return hit?.name ?? CUSTOM_PRESET;
}

interface FormState {
  preset: string;
  name: string;
  base_url: string;
  protocol: string;
  models: FormModel[];
  apiKey: string;
}

function formFromPreset(preset: Preset, t: (key: DictKey) => string): FormState {
  return {
    preset: preset.name,
    name: presetLabel(preset, t),
    base_url: preset.base_url,
    protocol: 'openai-compatible',
    models: [],
    apiKey: '',
  };
}

export default function AIConfigModal({ onClose, onSaved, showToast }: Props) {
  const t = useT();
  const [providers, setProviders] = useState<AiProvider[]>([]);
  const [rules, setRules] = useState<AiRule[]>([]);
  const [ruleInput, setRuleInput] = useState('');
  const [maxToolRounds, setMaxToolRounds] = useState(100);
  const [roundsSaving, setRoundsSaving] = useState(false);
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<AiProvider | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<AiProvider | null>(null);
  const [form, setForm] = useState<FormState>(() =>
    formFromPreset(PRESETS[0], t),
  );
  const [enabled, setEnabled] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);
  const [fetching, setFetching] = useState(false);
  const [showPicker, setShowPicker] = useState(false);
  const [remoteModels, setRemoteModels] = useState<RemoteAiModel[]>([]);
  const [pickerQuery, setPickerQuery] = useState('');
  const [pickerSelected, setPickerSelected] = useState<Set<string>>(new Set());

  const load = useCallback(async () => {
    const [list, rounds] = await Promise.all([
      listAiProviders(),
      getAiMaxToolRounds(),
    ]);
    setProviders(list);
    setMaxToolRounds(rounds);
  }, []);

  useEffect(() => {
    load().catch((e) => setError(fmtError(e)));
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
      setError(fmtError(err));
    }
  };

  const handleDeleteRule = async (id: string) => {
    try {
      await deleteAiRule(id);
      setRules((prev) => prev.filter((r) => r.id !== id));
    } catch (err) {
      setError(fmtError(err));
    }
  };

  const saveMaxToolRounds = async () => {
    const value = Number(maxToolRounds) || 0;
    if (value <= 0) {
      setError(t('ai.roundsInvalid'));
      return;
    }
    setRoundsSaving(true);
    try {
      await saveAiMaxToolRounds(value);
      setError(null);
      showToast('success', t('ai.roundsSaved', { n: value }));
    } catch (err) {
      setError(fmtError(err));
    } finally {
      setRoundsSaving(false);
    }
  };

  const openCreate = () => {
    setEditing(null);
    setForm(formFromPreset(PRESETS[0], t));
    setEnabled(true);
    setTestResult(null);
    setError(null);
    setShowPicker(false);
    setRemoteModels([]);
    setPickerSelected(new Set());
    setShowForm(true);
  };

  const openEdit = (p: AiProvider) => {
    setEditing(p);
    setForm({
      preset: presetOf(p.base_url),
      name: p.name,
      base_url: p.base_url,
      protocol: p.protocol,
      models: p.models.map((m) => ({
        id: m.id,
        label: m.label,
        model: m.model,
        is_active: m.is_active,
        context_window: m.context_window,
        supports_vision: m.supports_vision,
      })),
      apiKey: '',
    });
    setEnabled(p.enabled);
    setTestResult(null);
    setError(null);
    setShowPicker(false);
    setRemoteModels([]);
    setPickerSelected(new Set());
    setShowForm(true);
  };

  const applyPreset = (name: string) => {
    if (name === CUSTOM_PRESET) {
      // 清空下方所有字段，方便从零配置
      setForm({
        preset: CUSTOM_PRESET,
        name: '',
        base_url: '',
        protocol: 'openai-compatible',
        models: [],
        apiKey: '',
      });
      return;
    }
    const preset = PRESETS.find((p) => p.name === name);
    if (!preset) return;
    setForm((prev) => ({
      ...formFromPreset(preset, t),
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
          context_window: DEFAULT_CONTEXT_WINDOW,
          supports_vision: false,
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
        context_window: Number(m.context_window) || 0,
        supports_vision: m.supports_vision,
      }));
    if (!form.name.trim() || !form.base_url.trim()) {
      setError(t('ai.nameUrlRequired'));
      return;
    }
    if (models.length === 0) {
      setError(t('ai.needModel'));
      return;
    }
    const invalid = models.findIndex((m) => m.context_window <= 0);
    if (invalid >= 0) {
      setError(t('ai.invalidWindow', { n: invalid + 1 }));
      return;
    }
    setSaving(true);
    try {
      await saveAiProvider(
        {
          name: form.name.trim(),
          base_url: form.base_url.trim(),
          // 保留 protocol（编辑时后端默认值会覆盖为 openai-compatible，这里显式传回）
          protocol: form.protocol || 'openai-compatible',
          enabled,
          models,
          ...(form.apiKey.trim() ? { api_key: form.apiKey.trim() } : {}),
        },
        editing?.id,
      );
      setShowForm(false);
      await load();
      onSaved();
      showToast('success', t('ai.saved', { name: form.name.trim() }));
    } catch (err) {
      setError(fmtError(err));
    } finally {
      setSaving(false);
    }
  };

  const handleTest = async () => {
    if (!form.base_url.trim()) {
      setError(t('ai.baseUrlRequired'));
      return;
    }
    const model = activeModel();
    // 表单未填模型时，回退用最近拉取到的第一个模型，省一步
    const modelId = model?.model.trim() || remoteModels[0]?.id;
    if (!modelId) {
      setError(t('ai.needModelOrFetch'));
      return;
    }
    setTesting(true);
    setTestResult(null);
    try {
      const result = await testAiProvider({
        base_url: form.base_url.trim(),
        model: modelId,
        api_key: form.apiKey.trim() || undefined,
        id: editing?.id,
      });
      setTestResult(result);
    } catch (err) {
      setTestResult({ ok: false, message: fmtError(err) });
    } finally {
      setTesting(false);
    }
  };

  const handleFetchModels = async () => {
    if (!form.base_url.trim()) {
      setError(t('ai.baseUrlRequired'));
      return;
    }
    setError(null);
    setFetching(true);
    try {
      const list = await listRemoteAiModels({
        base_url: form.base_url.trim(),
        api_key: form.apiKey.trim() || undefined,
        id: editing?.id,
      });
      setRemoteModels(list);
      // 默认勾选当前表单中尚未存在的模型，避免重复导入
      const existing = new Set(form.models.map((m) => m.model.trim()));
      setPickerSelected(new Set(list.filter((m) => !existing.has(m.id)).map((m) => m.id)));
      setPickerQuery('');
      setShowPicker(true);
    } catch (err) {
      setError(fmtError(err));
    } finally {
      setFetching(false);
    }
  };

  const togglePickerSelect = (id: string) => {
    setPickerSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const filteredRemoteModels = remoteModels.filter((m) =>
    m.id.toLowerCase().includes(pickerQuery.trim().toLowerCase()),
  );

  const handleImportModels = () => {
    const existing = new Set(form.models.map((m) => m.model.trim()));
    const toAdd = remoteModels.filter(
      (m) => pickerSelected.has(m.id) && !existing.has(m.id),
    );
    if (toAdd.length === 0) {
      setShowPicker(false);
      return;
    }
    setForm((prev) => {
      const wasEmpty = prev.models.length === 0;
      const newModels = toAdd.map((m, idx) => ({
        label: m.id,
        model: m.id,
        is_active: wasEmpty && idx === 0,
        context_window: DEFAULT_CONTEXT_WINDOW,
        supports_vision: guessVisionModel(m.id),
      }));
      return { ...prev, models: [...prev.models, ...newModels] };
    });
    setShowPicker(false);
  };

  const handleDelete = (p: AiProvider) => {
    setDeleteTarget(p);
  };

  const doDelete = async (p: AiProvider) => {
    setDeleteTarget(null);
    try {
      await deleteAiProvider(p.id);
      await load();
      onSaved();
    } catch (err) {
      setError(fmtError(err));
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
            context_window: m.context_window,
            supports_vision: m.supports_vision,
          })),
        },
        p.id,
      );
      await load();
      onSaved();
    } catch (err) {
      setError(fmtError(err));
    }
  };

  return (
    <Modal
      title={t('ai.title')}
      subtitle={t('ai.subtitle')}
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal">
        {!showForm ? (
          <>
            <div className="ai-modal-actions">
              <button className="btn primary block" onClick={openCreate}>
                <PlusIcon size={16} /> {t('ai.newConfig')}
              </button>
            </div>

            {providers.length === 0 ? (
              <div className="ai-empty">
                <SparklesIcon size={30} />
                <p>{t('ai.emptyTitle')}</p>
                <span>{t('ai.emptyHint')}</span>
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
                            <CheckIcon size={11} /> {t('ai.enabled')}
                          </span>
                        ) : (
                          <span className="badge badge-off">{t('ai.disabled')}</span>
                        )}
                      </div>
                      <span className="provider-detail">
                        {t('ai.modelCount', { n: p.models.length, url: p.base_url })}
                      </span>
                      <div className="provider-model-chips">
                        {p.models.map((m) => (
                          <span
                            key={m.id}
                            className={`provider-model-chip${m.is_active ? ' active' : ''}`}
                            title={m.is_active ? t('ai.defaultModel') : m.model}
                          >
                            {m.label}
                          </span>
                        ))}
                      </div>
                    </div>
                    <div className="provider-actions">
                      {!p.enabled && (
                        <button className="btn ghost small" onClick={() => handleEnable(p)}>
                          {t('ai.enable')}
                        </button>
                      )}
                      <button className="btn ghost small" onClick={() => openEdit(p)}>
                        {t('ai.edit')}
                      </button>
                      <button
                        className="icon-btn danger"
                        title={t('ai.deleteConfig')}
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
                <span className="rules-title">{t('ai.rulesTitle')}</span>
                <span className="rules-hint">{t('ai.rulesHint')}</span>
              </div>
              <div className="rules-note">
                {t('ai.rulesNoteA')}
                <strong>{t('ai.rulesNoteB')}</strong>
                {t('ai.rulesNoteC')}
                <code>rm -rf</code>
                {t('ai.rulesNoteD')}
                <code>rm -rf /data</code>
                {t('ai.rulesNoteE')}
                <code>rm -rf /var/log/*</code>
                {t('ai.rulesNoteF')}
                <code>*</code>
                {t('ai.rulesNoteG')}
              </div>
              <div className="rules-list">
                {rules.length === 0 && (
                  <span className="rules-empty">{t('ai.rulesEmpty')}</span>
                )}
                {rules.map((r) => (
                  <div className="rule-chip" key={r.id}>
                    <code>{r.pattern}</code>
                    <button
                      className="rule-del"
                      title={t('ai.deleteRule')}
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
                  placeholder={t('ai.rulePlaceholder')}
                />
                <button className="btn secondary small" onClick={handleAddRule}>
                  {t('common.add')}
                </button>
              </div>
            </div>

            <div className="rules-section">
              <div className="rules-header">
                <span className="rules-title">{t('ai.roundsTitle')}</span>
                <span className="rules-hint">{t('ai.roundsHint')}</span>
              </div>
              <div className="rules-note">{t('ai.roundsNote')}</div>
              <div className="rule-add">
                <input
                  type="number"
                  min={1}
                  value={maxToolRounds}
                  onChange={(e) => setMaxToolRounds(Number(e.target.value) || 0)}
                />
                <button
                  className="btn secondary small"
                  onClick={saveMaxToolRounds}
                  disabled={roundsSaving}
                >
                  {roundsSaving ? t('common.saving') : t('common.save')}
                </button>
              </div>
            </div>
          </>
        ) : (
          <form className="ai-form" onSubmit={handleSave}>
            <div className="ai-field">
              <span className="ai-field-label">{t('ai.preset')}</span>
              <Select
                value={form.preset}
                options={[
                  ...PRESETS.map((p) => ({
                    value: p.name,
                    label: presetLabel(p, t),
                  })),
                  { value: CUSTOM_PRESET, label: t('ai.presetCustom') },
                ]}
                onChange={applyPreset}
                ariaLabel={t('ai.preset')}
              />
            </div>
            <label>
              {t('ai.name')}
              <input
                value={form.name}
                onChange={(e) => setForm({ ...form, name: e.target.value })}
                placeholder={t('ai.namePlaceholder')}
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
                    ? t('ai.apiKeyPlaceholderEdit')
                    : t('ai.apiKeyPlaceholderNew')
                }
              />
            </label>

            <div className="model-editor">
              <div className="model-editor-header">
                <span className="model-editor-label">{t('ai.modelList')}</span>
                <div className="model-editor-actions">
                  <button
                    type="button"
                    className="btn secondary small"
                    onClick={handleFetchModels}
                    disabled={fetching}
                    title={t('ai.fetchModelsTitle')}
                  >
                    <DownloadIcon size={13} />
                    {fetching ? t('ai.fetching') : t('ai.fetchModels')}
                  </button>
                  <button
                    type="button"
                    className="btn secondary small add-model"
                    onClick={addModel}
                  >
                    <PlusIcon size={13} /> {t('ai.addModel')}
                  </button>
                </div>
              </div>

              {showPicker && (
                <div className="model-picker">
                  <div className="model-picker-bar">
                    <input
                      className="model-picker-search"
                      placeholder={t('ai.searchModels', { n: remoteModels.length })}
                      value={pickerQuery}
                      onChange={(e) => setPickerQuery(e.target.value)}
                      autoFocus
                    />
                    <button
                      type="button"
                      className="btn ghost small"
                      onClick={() =>
                        setPickerSelected((prev) =>
                          prev.size === filteredRemoteModels.length
                            ? new Set()
                            : new Set(filteredRemoteModels.map((m) => m.id)),
                        )
                      }
                    >
                      {pickerSelected.size === filteredRemoteModels.length &&
                      filteredRemoteModels.length > 0
                        ? t('ai.selectNone')
                        : t('ai.selectAll')}
                    </button>
                  </div>
                  <div className="model-picker-list">
                    {filteredRemoteModels.length === 0 ? (
                      <div className="model-picker-empty">{t('ai.noMatchModels')}</div>
                    ) : (
                      filteredRemoteModels.map((m) => {
                        const exists = form.models.some(
                          (fm) => fm.model.trim() === m.id,
                        );
                        const checked = pickerSelected.has(m.id);
                        return (
                          <label
                            key={m.id}
                            className={`model-picker-item${checked ? ' checked' : ''}${
                              exists ? ' exists' : ''
                            }`}
                          >
                            <input
                              type="checkbox"
                              checked={checked}
                              disabled={exists}
                              onChange={() => togglePickerSelect(m.id)}
                            />
                            <span className="model-picker-id">{m.id}</span>
                            {m.owned_by && (
                              <span className="model-picker-owner">{m.owned_by}</span>
                            )}
                            {exists && (
                              <span className="model-picker-tag">{t('ai.added')}</span>
                            )}
                          </label>
                        );
                      })
                    )}
                  </div>
                  <div className="model-picker-footer">
                    <span className="model-picker-count">
                      {t('ai.selectedCount', { n: pickerSelected.size })}
                    </span>
                    <div className="model-picker-footer-actions">
                      <button
                        type="button"
                        className="btn ghost small"
                        onClick={() => setShowPicker(false)}
                      >
                        {t('common.cancel')}
                      </button>
                      <button
                        type="button"
                        className="btn primary small"
                        onClick={handleImportModels}
                        disabled={pickerSelected.size === 0}
                      >
                        {t('ai.importN', {
                          n: pickerSelected.size > 0 ? pickerSelected.size : '',
                        })}
                      </button>
                    </div>
                  </div>
                </div>
              )}

              {form.models.map((m, idx) => (
                <div className="model-row" key={m.id ?? `model-${idx}`}>
                  <input
                    value={m.label}
                    placeholder={t('ai.modelLabelPlaceholder')}
                    onChange={(e) => updateModel(idx, { label: e.target.value })}
                  />
                  <input
                    value={m.model}
                    placeholder={t('ai.modelIdPlaceholder')}
                    className="model-id-input"
                    onChange={(e) => updateModel(idx, { model: e.target.value })}
                  />
                  <div className="model-window">
                    <input
                      type="number"
                      min={1}
                      className="model-window-input"
                      value={m.context_window || ''}
                      placeholder={t('ai.contextPlaceholder')}
                      onChange={(e) =>
                        updateModel(idx, {
                          context_window: Number(e.target.value) || 0,
                        })
                      }
                    />
                    <div className="model-window-quick">
                      {[64_000, 128_000, 200_000, 1_000_000].map((v) => (
                        <button
                          key={v}
                          type="button"
                          onClick={() => updateModel(idx, { context_window: v })}
                          title={t('ai.fillTokens', { n: v.toLocaleString() })}
                        >
                          {v >= 1_000_000 ? '1M' : `${v / 1000}k`}
                        </button>
                      ))}
                    </div>
                  </div>
                  <label
                    className={`model-vision${m.supports_vision ? ' on' : ''}`}
                    title={t('ai.visionTitle')}
                  >
                    <input
                      type="checkbox"
                      checked={m.supports_vision}
                      onChange={(e) =>
                        updateModel(idx, { supports_vision: e.target.checked })
                      }
                    />
                    {t('ai.vision')}
                  </label>
                  <button
                    type="button"
                    className={`model-default${m.is_active ? ' on' : ''}`}
                    title={t('ai.defaultModel')}
                    onClick={() => setActiveModel(idx)}
                  >
                    {m.is_active ? t('common.default') : t('ai.setDefault')}
                  </button>
                  <button
                    type="button"
                    className="icon-btn danger"
                    title={t('ai.deleteModel')}
                    onClick={() => removeModel(idx)}
                  >
                    <TrashIcon size={14} />
                  </button>
                </div>
              ))}
            </div>

            <div className="form-row-inline">
              <span className="form-label">{t('ai.setActive')}</span>
              <button
                type="button"
                className={`switch${enabled ? ' on' : ''}`}
                onClick={() => setEnabled(!enabled)}
                aria-label={t('ai.toggleAria')}
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
                {t('common.cancel')}
              </button>
              <button
                type="button"
                className="btn secondary"
                onClick={handleTest}
                disabled={testing}
              >
                {testing ? t('common.testing') : t('ai.testConnection')}
              </button>
              <button type="submit" className="btn primary" disabled={saving}>
                {saving ? t('common.saving') : t('common.save')}
              </button>
            </div>
          </form>
        )}
      </div>

      {deleteTarget && (
        <ConfirmModal
          title={t('ai.deleteTitle')}
          body={t('ai.deleteBody', { name: deleteTarget.name })}
          confirmText={t('common.delete')}
          danger
          onConfirm={() => doDelete(deleteTarget)}
          onCancel={() => setDeleteTarget(null)}
        />
      )}
    </Modal>
  );
}
