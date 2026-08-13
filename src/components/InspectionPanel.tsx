import { useCallback, useEffect, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import {
  cancelInspection,
  deleteInspectionReport,
  getAlertSettings,
  getInspectionReport,
  listInspectionReports,
  onInspectionDone,
  onInspectionError,
  onInspectionProgress,
  startInspection,
} from '../api';
import type {
  InspectionProgressPayload,
  InspectionReport,
  InspectionStatus,
} from '../types';
import type { Host } from '../types';
import { InspectIcon, RefreshIcon, StopIcon, TrashIcon, XIcon } from './Icons';

interface Props {
  host: Host;
  onClose: () => void;
}

const RISK_LABEL: Record<string, string> = {
  low: '低风险',
  medium: '中风险',
  high: '高风险',
  unknown: '未知',
};

export default function InspectionPanel({ host, onClose }: Props) {
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [status, setStatus] = useState<InspectionStatus | null>(null);
  const [progress, setProgress] =
    useState<InspectionProgressPayload | null>(null);
  const [steps, setSteps] = useState<string[]>([]);
  const [report, setReport] = useState<InspectionReport | null>(null);
  const [history, setHistory] = useState<InspectionReport[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [emailConfigured, setEmailConfigured] = useState<boolean | null>(null);
  const currentIdRef = useRef<string | null>(null);
  const runningRef = useRef(false);

  const refreshHistory = useCallback(async () => {
    try {
      setHistory(await listInspectionReports(host.id, 30));
    } catch {
      // 历史加载失败不影响当前巡检
    }
  }, [host.id]);

  const finishReport = useCallback(
    async (id: string) => {
      try {
        const next = await getInspectionReport(id);
        if (next) {
          setReport(next);
          setStatus(next.status);
          setError(next.error ?? null);
        } else {
          setStatus('failed');
          setError('未找到巡检报告');
        }
      } catch (e) {
        setStatus('failed');
        setError(String(e));
      }
      await refreshHistory();
    },
    [refreshHistory],
  );

  const begin = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    setStatus('running');
    setReport(null);
    setError(null);
    setProgress(null);
    setSteps([]);
    try {
      const id = await startInspection(host);
      currentIdRef.current = id;
      setCurrentId(id);
    } catch (e) {
      setStatus('failed');
      setError(String(e));
    } finally {
      runningRef.current = false;
    }
  }, [host]);

  useEffect(() => {
    refreshHistory().catch(() => {});
    getAlertSettings()
      .then((s) => {
        setEmailConfigured(
          !!s.smtp_host?.trim() && !!s.smtp_to?.trim(),
        );
      })
      .catch(() => setEmailConfigured(false));
  }, [refreshHistory]);

  useEffect(() => {
    let cancelled = false;
    let unProgress: (() => void) | undefined;
    let unDone: (() => void) | undefined;
    let unError: (() => void) | undefined;

    onInspectionProgress((p) => {
      if (p.report_id !== currentIdRef.current) return;
      setProgress(p);
      if (p.phase === 'exec') {
        setSteps((prev) => [...prev, p.message]);
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unProgress = fn;
    });

    onInspectionDone((p) => {
      if (p.report_id !== currentIdRef.current) return;
      finishReport(p.report_id);
    }).then((fn) => {
      if (cancelled) fn();
      else unDone = fn;
    });

    onInspectionError((p) => {
      if (p.report_id !== currentIdRef.current) return;
      finishReport(p.report_id);
    }).then((fn) => {
      if (cancelled) fn();
      else unError = fn;
    });

    return () => {
      cancelled = true;
      unProgress?.();
      unDone?.();
      unError?.();
    };
  }, [finishReport]);

  const handleCancel = async () => {
    if (!currentIdRef.current) return;
    await cancelInspection(currentIdRef.current).catch(() => {});
  };

  const handleDeleteReport = async (id: string) => {
    if (!window.confirm('确定删除这条巡检报告吗？')) return;
    await deleteInspectionReport(id).catch((e) => {
      setError(String(e));
    });
    if (currentIdRef.current === id) {
      currentIdRef.current = null;
      setCurrentId(null);
      setReport(null);
      setStatus(null);
      setError(null);
      setProgress(null);
      setSteps([]);
    }
    await refreshHistory().catch(() => {});
  };

  const openReport = async (id: string) => {
    const next = await getInspectionReport(id).catch(() => null);
    if (next) {
      setReport(next);
      setStatus(next.status);
      setError(next.error ?? null);
      currentIdRef.current = id;
      setCurrentId(id);
    }
  };

  return (
    <aside className="inspection-panel">
      <div className="inspection-header">
        <div className="inspection-title">
          <InspectIcon size={16} />
          <span>AI 巡检 · {host.name}</span>
        </div>
        <div className="inspection-actions">
          {status === 'running' && (
            <button className="icon-btn" title="取消巡检" onClick={handleCancel}>
              <StopIcon size={14} />
            </button>
          )}
          <div
            className={`inspection-mail-state${emailConfigured === false ? ' warn' : ''}`}
            title={
              emailConfigured === false
                ? '未配置邮件通知，巡检完成后不会发送邮件报告'
                : emailConfigured === true
                  ? '邮件通知已配置'
                  : '正在检查邮件通知配置'
            }
          >
            {emailConfigured === false
              ? '未配置邮件通知'
              : emailConfigured === true
                ? '邮件通知已配置'
                : '邮件通知检查中'}
          </div>
          <button className="icon-btn" title="关闭" onClick={onClose}>
            <XIcon size={14} />
          </button>
        </div>
      </div>

      <div className="inspection-body">
        {status === null && (
          <div className="inspection-start">
            <div className="inspection-start-icon">
              <InspectIcon size={28} />
            </div>
            <div className="inspection-start-text">
              对 <strong>{host.name}</strong> 执行一次 AI 只读巡检
            </div>
            <button className="btn primary" onClick={begin}>
              <InspectIcon size={15} /> 开始巡检
            </button>
          </div>
        )}

        {status === 'running' && (
          <div className="inspection-running">
            <div className="spinner" />
            <div>
              <div className="inspection-phase">
                {progress?.message ?? '正在准备巡检…'}
              </div>
              {steps.length > 0 && (
                <div className="inspection-steps">
                  {steps.slice(-5).map((step, idx) => (
                    <div key={`${idx}-${step}`} className="inspection-step">
                      {step}
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        )}

        {error && status !== 'running' && (
          <div className="inspection-error">{error}</div>
        )}

        {report && (
          <div className="inspection-report">
            <div className="inspection-meta">
              <span className={`risk-badge risk-${report.risk_level}`}>
                {RISK_LABEL[report.risk_level] ?? report.risk_level}
              </span>
              <span>{report.provider_name} · {report.model}</span>
              <span>
                {report.duration_ms != null
                  ? `${(report.duration_ms / 1000).toFixed(1)}s`
                  : '—'}
              </span>
              <span>{report.email_sent ? '✓ 邮件已发送' : '邮件未发送'}</span>
              <button className="btn secondary small" onClick={begin}>
                <RefreshIcon size={13} /> 重新巡检
              </button>
            </div>
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {report.markdown}
            </ReactMarkdown>
          </div>
        )}

        <div className="inspection-history">
          <div className="inspection-history-title">历史报告</div>
          {history.length === 0 && (
            <div className="inspection-empty">暂无巡检记录</div>
          )}
          {history.map((item) => (
            <button
              key={item.id}
              className={`inspection-history-item${currentId === item.id ? ' active' : ''}`}
              onClick={() => openReport(item.id)}
            >
              <span className={`risk-dot risk-${item.risk_level}`} />
              <span className="inspection-history-main">
                <span className="inspection-history-host">{item.host_label}</span>
                <span className="inspection-history-summary">{item.summary}</span>
              </span>
              <span className="inspection-history-time">
                {new Date(item.created_at * 1000).toLocaleString()}
              </span>
              <button
                className="icon-btn danger inspection-delete"
                title="删除报告"
                onClick={(e) => {
                  e.stopPropagation();
                  handleDeleteReport(item.id);
                }}
              >
                <TrashIcon size={14} />
              </button>
            </button>
          ))}
        </div>
      </div>
    </aside>
  );
}
