import { useCallback, useEffect, useRef, useState } from 'react';
import { Compartment, EditorState } from '@codemirror/state';
import { EditorView, keymap } from '@codemirror/view';
import { basicSetup } from 'codemirror';
import { StreamLanguage } from '@codemirror/language';
import { oneDark } from '@codemirror/theme-one-dark';
import { javascript } from '@codemirror/lang-javascript';
import { json } from '@codemirror/lang-json';
import { python } from '@codemirror/lang-python';
import { markdown } from '@codemirror/lang-markdown';
import { yaml } from '@codemirror/lang-yaml';
import { html } from '@codemirror/lang-html';
import { css } from '@codemirror/lang-css';
import { xml } from '@codemirror/lang-xml';
import { sql } from '@codemirror/lang-sql';
import { shell } from '@codemirror/legacy-modes/mode/shell';
import { nginx } from '@codemirror/legacy-modes/mode/nginx';
import { properties } from '@codemirror/legacy-modes/mode/properties';
import { dockerFile } from '@codemirror/legacy-modes/mode/dockerfile';
import { sftpReadFile, sftpWriteFile } from '../api';
import { useT } from '../i18n';
import type { Host } from '../types';
import { fmtError } from '../utils/errors';
import { baseName, formatBytes, formatMtime } from '../utils/remote';
import ConfirmModal from './ConfirmModal';
import { FolderIcon, RefreshIcon, SaveIcon, XIcon } from './Icons';
import type { ToastItem } from './Toast';

interface Props {
  host: Host;
  /** 远端文件绝对路径 */
  path: string;
  tabKey: number;
  theme: 'dark' | 'light';
  /** 右侧文件面板是否展开（编辑文件时仍可继续浏览目录） */
  sftpOpen: boolean;
  onToggleSftp: () => void;
  /** 脏状态变化：供标签栏展示与关闭前确认使用 */
  onDirtyChange: (tabKey: number, dirty: boolean) => void;
  onClose: () => void;
  onNotify: (kind: ToastItem['kind'], message: string) => void;
}

/** 统一换行符后再比较：CodeMirror 按行存储文档，CRLF 文件读出来会被规整成 LF，
 *  直接比较原文会误判为「已修改」 */
function normalizeEol(text: string): string {
  return text.replace(/\r\n?/g, '\n');
}

/** 按文件名后缀选择语法高亮；无法识别时退化为纯文本 */
function languageExtension(path: string) {
  const name = baseName(path).toLowerCase();
  const ext = name.includes('.') ? name.slice(name.lastIndexOf('.') + 1) : '';
  switch (ext) {
    case 'js':
    case 'jsx':
    case 'mjs':
    case 'cjs':
    case 'ts':
    case 'tsx':
      return javascript({ jsx: true, typescript: ext.startsWith('ts') });
    case 'json':
    case 'jsonc':
      return json();
    case 'py':
    case 'pyw':
      return python();
    case 'md':
    case 'markdown':
      return markdown();
    case 'yaml':
    case 'yml':
      return yaml();
    case 'html':
    case 'htm':
      return html();
    case 'css':
    case 'scss':
    case 'less':
      return css();
    case 'xml':
    case 'svg':
      return xml();
    case 'sql':
      return sql();
    case 'sh':
    case 'bash':
    case 'zsh':
    case 'ksh':
    case 'bashrc':
    case 'profile':
      return StreamLanguage.define(shell);
    case 'conf':
    case 'cfg':
    case 'ini':
    case 'properties':
    case 'env':
    case 'service':
    case 'timer':
    case 'socket':
      return StreamLanguage.define(properties);
    default:
      break;
  }
  if (name === 'dockerfile' || name.startsWith('dockerfile.')) {
    return StreamLanguage.define(dockerFile);
  }
  if (name.startsWith('nginx') || path.includes('/nginx/')) {
    return StreamLanguage.define(nginx);
  }
  return [];
}

/** 远端文本文件编辑器：CodeMirror 6 + 原子保存 + 并发修改检测 */
export default function FileEditor({
  host,
  path,
  tabKey,
  theme,
  sftpOpen,
  onToggleSftp,
  onDirtyChange,
  onClose,
  onNotify,
}: Props) {
  const t = useT();
  const [loading, setLoading] = useState(true);
  /** 加载失败原因（大小超限 / 二进制 / 权限等） */
  const [loadError, setLoadError] = useState<string | null>(null);
  /** 保存失败原因；命中「远端已被修改」时展示重新加载入口 */
  const [saveError, setSaveError] = useState<string | null>(null);
  const [doc, setDoc] = useState<string | null>(null);
  /** 每次从远端成功加载递增：用于区分「重新加载」与无关重渲染 */
  const [docSeq, setDocSeq] = useState(0);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [size, setSize] = useState(0);
  const [mtime, setMtime] = useState(0);
  const [sensitive, setSensitive] = useState(false);
  /** 敏感路径保存确认：非 null 表示待确认的待写内容 */
  const [pendingContent, setPendingContent] = useState<string | null>(null);
  const [confirmReload, setConfirmReload] = useState(false);

  const containerRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<EditorView | null>(null);
  /** 最近一次「已保存/已加载」的内容基线（统一换行后），用于计算脏状态 */
  const baselineRef = useRef('');
  /** 原文件使用的换行符：保存时按原样写回，避免把 CRLF 文件改成 LF */
  const eolRef = useRef('\n');
  /** 最近一次灌入编辑器的加载序号：避免无关重渲染把用户的编辑覆盖回加载态 */
  const appliedSeqRef = useRef(0);
  /** 打开时记录的远端 mtime，保存时回传做并发修改检测 */
  const mtimeRef = useRef<number | null>(null);
  const themeCompartment = useRef(new Compartment());
  const dirtyRef = useRef(false);
  const saveRef = useRef<() => void>(() => {});

  const load = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    setSaveError(null);
    try {
      const res = await sftpReadFile(host.id, path);
      baselineRef.current = normalizeEol(res.content);
      eolRef.current = res.content.includes('\r\n') ? '\r\n' : '\n';
      // 服务端未返回 mtime 时置空，避免后续保存被误判为「远端已被修改」
      mtimeRef.current = res.mtime || null;
      setSize(res.size);
      setMtime(res.mtime);
      setSensitive(res.sensitive);
      dirtyRef.current = false;
      setDirty(false);
      onDirtyChange(tabKey, false);
      setDoc(res.content);
      setDocSeq((s) => s + 1);
    } catch (e) {
      setLoadError(fmtError(e));
      setDoc(null);
    } finally {
      setLoading(false);
    }
  }, [host.id, path, tabKey, onDirtyChange]);

  useEffect(() => {
    void load();
  }, [load]);

  // 编辑器实例：内容首次到达时创建，重新加载时整体替换文档。
  // 只有「新的加载结果」（docSeq 递增）才重灌文档——主题切换等无关重渲染
  // 不会覆盖用户尚未保存的编辑。
  useEffect(() => {
    const parent = containerRef.current;
    if (!parent || doc === null) return;
    if (!viewRef.current) {
      const view = new EditorView({
        state: EditorState.create({
          doc,
          extensions: [
            basicSetup,
            languageExtension(path),
            themeCompartment.current.of(theme === 'dark' ? oneDark : []),
            EditorView.lineWrapping,
            keymap.of([
              {
                key: 'Mod-s',
                preventDefault: true,
                run: () => {
                  saveRef.current();
                  return true;
                },
              },
            ]),
            EditorView.updateListener.of((update) => {
              if (!update.docChanged) return;
              const next = normalizeEol(update.state.doc.toString());
              const isDirty = next !== baselineRef.current;
              if (isDirty === dirtyRef.current) return;
              dirtyRef.current = isDirty;
              setDirty(isDirty);
              onDirtyChange(tabKey, isDirty);
            }),
          ],
        }),
        parent,
      });
      viewRef.current = view;
      appliedSeqRef.current = docSeq;
    } else if (appliedSeqRef.current !== docSeq) {
      const view = viewRef.current;
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: doc },
      });
      appliedSeqRef.current = docSeq;
    }
  }, [doc, docSeq, path, tabKey, onDirtyChange]);

  // 主题切换：只重配置配色，不重建编辑器（保留撤销历史与光标）
  useEffect(() => {
    viewRef.current?.dispatch({
      effects: themeCompartment.current.reconfigure(theme === 'dark' ? oneDark : []),
    });
  }, [theme]);

  useEffect(
    () => () => {
      viewRef.current?.destroy();
      viewRef.current = null;
      appliedSeqRef.current = 0;
    },
    [],
  );

  const doSave = useCallback(
    async (content: string, confirmed: boolean) => {
      setSaving(true);
      setSaveError(null);
      try {
        const res = await sftpWriteFile(
          host.id,
          path,
          content,
          mtimeRef.current,
          confirmed,
        );
        if (res.requires_confirm) {
          setPendingContent(content);
          return;
        }
        if (!res.ok) {
          setSaveError(res.text || t('editor.saveFailed'));
          return;
        }
        baselineRef.current = normalizeEol(content);
        mtimeRef.current = res.mtime || null;
        setMtime(res.mtime || mtime);
        setSize(new Blob([content]).size);
        dirtyRef.current = false;
        setDirty(false);
        onDirtyChange(tabKey, false);
        onNotify('success', t('editor.savedToast', { name: baseName(path) }));
      } catch (e) {
        const message = fmtError(e);
        setSaveError(message);
        onNotify('error', message);
      } finally {
        setSaving(false);
      }
    },
    [host.id, path, mtime, onDirtyChange, onNotify, tabKey, t],
  );

  // 编辑器内容按原文件换行符取回；未修改时不触发写入（避免无谓的远端写与审计记录）
  saveRef.current = () => {
    if (!dirtyRef.current) return;
    const view = viewRef.current;
    if (!view) return;
    const text = view.state.doc.toString();
    void doSave(
      eolRef.current === '\r\n' ? text.replace(/\n/g, '\r\n') : text,
      false,
    );
  };

  // Ctrl/Cmd+S 兜底：编辑器未聚焦（如刚点过工具栏）时同样触发保存
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.key.toLowerCase() !== 's') return;
      if (containerRef.current?.offsetParent === null) return; // 非激活标签不响应
      e.preventDefault();
      saveRef.current();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const reload = () => {
    setConfirmReload(false);
    void load();
  };

  return (
    <div className="file-editor">
      <div className="file-editor-header">
        <span className="file-editor-path" title={path}>
          {path}
          {sensitive && (
            <span
              className="file-editor-badge"
              title={t('editor.sensitiveTitle')}
            >
              {t('editor.sensitive')}
            </span>
          )}
        </span>
        <div className="file-editor-actions">
          <button
            className="btn primary small"
            onClick={() => saveRef.current()}
            disabled={!dirty || saving || doc === null}
            title={t('editor.saveTitle')}
          >
            <SaveIcon size={13} />
            {saving
              ? t('common.saving')
              : dirty
                ? t('common.save')
                : t('editor.saved')}
          </button>
          <button
            className="btn ghost small"
            onClick={() => (dirty ? setConfirmReload(true) : reload())}
            disabled={loading || saving}
            title={t('editor.reloadTitle')}
          >
            <RefreshIcon size={13} /> {t('editor.reload')}
          </button>
          <button
            className={`btn ghost small${sftpOpen ? ' active' : ''}`}
            onClick={onToggleSftp}
            title={sftpOpen ? t('editor.collapseSftp') : t('editor.expandSftp')}
          >
            <FolderIcon size={13} /> {t('editor.files')}
          </button>
          <button className="icon-btn" title={t('common.close')} onClick={onClose}>
            <XIcon size={14} />
          </button>
        </div>
      </div>

      <div className="file-editor-status">
        {loading && <span>{t('common.loading')}</span>}
        {!loading && loadError && <span className="err">{loadError}</span>}
        {!loading && !loadError && (
          <>
            <span>{formatBytes(size)}</span>
            <span>{t('editor.modifiedAt', { time: formatMtime(mtime) })}</span>
            {dirty && <span className="dirty">{t('editor.unsaved')}</span>}
            {saving && <span>{t('editor.writing')}</span>}
          </>
        )}
        {saveError && (
          <span className="err file-editor-save-error">
            {saveError}
            <button className="btn ghost small" onClick={reload} disabled={saving}>
              {t('editor.reload')}
            </button>
          </span>
        )}
      </div>

      <div className="file-editor-body" ref={containerRef}>
        {!loading && loadError && (
          <div className="file-editor-placeholder">
            <p>{t('editor.cannotOpen')}</p>
            <span>{t('editor.cannotOpenHint')}</span>
          </div>
        )}
      </div>

      {pendingContent !== null && (
        <ConfirmModal
          title={t('editor.sensitiveWriteTitle')}
          body={t('editor.sensitiveWriteBody', { path })}
          confirmText={t('editor.confirmWrite')}
          danger
          onConfirm={() => {
            const content = pendingContent;
            setPendingContent(null);
            void doSave(content, true);
          }}
          onCancel={() => setPendingContent(null)}
        />
      )}

      {confirmReload && (
        <ConfirmModal
          title={t('editor.reload')}
          body={t('editor.reloadBody')}
          confirmText={t('editor.discardReload')}
          danger
          onConfirm={reload}
          onCancel={() => setConfirmReload(false)}
        />
      )}
    </div>
  );
}
