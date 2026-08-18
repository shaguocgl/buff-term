import { useCallback, useEffect, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import {
  cancelInspection,
  cancelRemediation,
  deleteInspectionReport,
  executeRemediation,
  getAlertSettings,
  getInspectionReport,
  getRemediation,
  listInspectionReports,
  onInspectionDone,
  onInspectionError,
  onInspectionProgress,
  onRemediationDone,
  onRemediationError,
  onRemediationProgress,
  retryRemediation,
  startInspection,
  startRemediationPlanning,
} from '../api';
import type {
  InspectionProgressPayload,
  InspectionReport,
  InspectionStatus,
  Remediation,
  RemediationProgressPayload,
  RemediationStepInput,
} from '../types';
import type { Host } from '../types';
import {
  CheckIcon,
  InspectIcon,
  PlusIcon,
  RefreshIcon,
  StopIcon,
  TrashIcon,
  WrenchIcon,
  XIcon,
} from './Icons';
import Modal from './Modal';

interface Props {
  host: Host;
  panelWidth?: number;
  onClose: () => void;
}

const RISK_LABEL: Record<string, string> = {
  low: '低风险',
  medium: '中风险',
  high: '高风险',
  unknown: '未知',
};

export default function InspectionPanel({ host, panelWidth = 620, onClose }: Props) {
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
  const [remediation, setRemediation] = useState<Remediation | null>(null);
  const [remediationBusy, setRemediationBusy] = useState(false);
  const [remediationError, setRemediationError] = useState<string | null>(null);
  const [remediationProgress, setRemediationProgress] =
    useState<RemediationProgressPayload | null>(null);
  const [intervention, setIntervention] = useState('');
  const [editSteps, setEditSteps] = useState<RemediationStepInput[]>([]);
  const [remediationOpen, setRemediationOpen] = useState(false);
  const [pendingDangerSteps, setPendingDangerSteps] = useState<
    RemediationStepInput[]
  >([]);
  const [confirmingDanger, setConfirmingDanger] = useState(false);
  const [dangerChecked, setDangerChecked] = useState(false);
  const remediationIdRef = useRef<string | null>(null);
  const remediationBoxRef = useRef<HTMLDivElement | null>(null);

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
          loadRemediation(id).catch(() => {});
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

  const loadRemediation = useCallback(async (reportId: string) => {
    const next = await getRemediation(reportId).catch(() => null);
    setRemediation(next);
    remediationIdRef.current = next?.id ?? null;
    setRemediationOpen(next != null);
    if (next) {
      setIntervention(next.intervention);
      setEditSteps(
        next.steps.map((s) => ({
          description: s.description,
          command: s.command,
          timeout_secs: s.timeout_secs,
        })),
      );
    } else {
      setIntervention('');
      setEditSteps([]);
    }
  }, []);

  const begin = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    setStatus('running');
    setReport(null);
    setError(null);
    setProgress(null);
    setSteps([]);
    setRemediation(null);
    setRemediationError(null);
    setRemediationProgress(null);
    setRemediationBusy(false);
    remediationIdRef.current = null;
    setIntervention('');
    setEditSteps([]);
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

  useEffect(() => {
    let cancelled = false;
    let unProgress: (() => void) | undefined;
    let unDone: (() => void) | undefined;
    let unError: (() => void) | undefined;

    onRemediationProgress((p) => {
      if (p.remediation_id !== remediationIdRef.current) return;
      setRemediationProgress(p);
      if (
        p.phase === 'step_start' ||
        p.phase === 'step_success' ||
        p.phase === 'step_error'
      ) {
        const reportId = currentIdRef.current;
        if (reportId) loadRemediation(reportId).catch(() => {});
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unProgress = fn;
    });

    onRemediationDone((p) => {
      if (p.remediation_id !== remediationIdRef.current) return;
      setRemediationBusy(false);
      setRemediationProgress(null);
      const reportId = currentIdRef.current;
      if (reportId) loadRemediation(reportId).catch(() => {});
    }).then((fn) => {
      if (cancelled) fn();
      else unDone = fn;
    });

    onRemediationError((p) => {
      if (p.remediation_id !== remediationIdRef.current) return;
      setRemediationBusy(false);
      setRemediationError(p.message);
      setRemediationProgress(null);
      const reportId = currentIdRef.current;
      if (reportId) loadRemediation(reportId).catch(() => {});
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
  }, [loadRemediation]);

  useEffect(() => {
    if (!remediationOpen) return;
    const id = requestAnimationFrame(() => {
      remediationBoxRef.current?.scrollIntoView({
        behavior: 'smooth',
        block: 'start',
      });
    });
    return () => cancelAnimationFrame(id);
  }, [remediationOpen]);

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

  const isDangerousCommand = (cmd: string) => {
    const c = cmd.toLowerCase();
    return [
      'rm -rf',
      'rm -fr',
      'mkfs',
      'dd if=',
      'iptables',
      'systemctl stop',
      'systemctl restart',
      'systemctl disable',
      'systemctl mask',
      'shutdown',
      'reboot',
      'poweroff',
      'chmod -r',
      'chown -r',
      'fdisk',
      'parted',
      'userdel',
      'groupdel',
      'drop database',
      'truncate table',
      'delete from',
      'kill -9',
    ].some((p) => c.includes(p));
  };

  const updateStep = (
    index: number,
    patch: Partial<RemediationStepInput>,
  ) => {
    setEditSteps((prev) =>
      prev.map((step, i) => (i === index ? { ...step, ...patch } : step)),
    );
  };

  const removeStep = (index: number) => {
    setEditSteps((prev) => prev.filter((_, i) => i !== index));
  };

  const addStep = () => {
    setEditSteps((prev) => [
      ...prev,
      { description: '', command: '', timeout_secs: 60 },
    ]);
  };

  const handleGenerateRemediation = async () => {
    if (!report || remediationBusy) return;
    setRemediationBusy(true);
    setRemediationError(null);
    setRemediationProgress({
      remediation_id: '',
      phase: 'planning',
      message: 'AI 正在生成整改步骤…',
    });
    try {
      const id = await startRemediationPlanning(report.id, intervention);
      remediationIdRef.current = id;
      setRemediationProgress((prev) =>
        prev ? { ...prev, remediation_id: id } : prev,
      );
    } catch (e) {
      setRemediationBusy(false);
      setRemediationError(String(e));
    }
  };

  const doExecute = async (steps: RemediationStepInput[]) => {
    if (!remediationIdRef.current) return;
    setRemediationBusy(true);
    setRemediationError(null);
    setRemediationProgress(null);
    try {
      await executeRemediation(remediationIdRef.current, steps);
    } catch (e) {
      setRemediationBusy(false);
      setRemediationError(String(e));
    }
  };

  const handleExecute = () => {
    const steps = editSteps.filter((s) => s.command.trim());
    if (steps.length === 0) {
      setRemediationError('请至少保留一条可执行的整改步骤');
      return;
    }
    if (steps.some((s) => isDangerousCommand(s.command))) {
      setPendingDangerSteps(steps);
      setDangerChecked(false);
      setConfirmingDanger(true);
    } else {
      doExecute(steps);
    }
  };

  const confirmExecuteDanger = () => {
    const steps = pendingDangerSteps;
    setConfirmingDanger(false);
    if (dangerChecked && steps.length > 0) {
      doExecute(steps);
    }
  };

  const handleCancelRemediation = async () => {
    if (!remediationIdRef.current) return;
    await cancelRemediation(remediationIdRef.current).catch(() => {});
  };

  const handleRetryRemediation = async () => {
    if (!remediationIdRef.current) return;
    setRemediationBusy(true);
    setRemediationError(null);
    setRemediationProgress(null);
    try {
      await retryRemediation(remediationIdRef.current);
    } catch (e) {
      setRemediationBusy(false);
      setRemediationError(String(e));
    }
  };

  const openReport = async (id: string) => {
    const next = await getInspectionReport(id).catch(() => null);
    if (next) {
      setReport(next);
      setStatus(next.status);
      setError(next.error ?? null);
      currentIdRef.current = id;
      setCurrentId(id);
      setRemediationProgress(null);
      loadRemediation(id).catch(() => {});
    }
  };

  return (
    <aside className="inspection-panel" style={{ width: panelWidth }}>
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
              <div className="inspection-meta-info">
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
              </div>
              <div className="inspection-meta-actions">
                <button className="btn secondary inspection-reinspect" onClick={begin}>
                  <RefreshIcon size={14} /> 重新巡检
                </button>
                {remediation?.status === 'success' ? (
                  <span className="remediation-done-badge">
                    <CheckIcon size={12} /> 已整改
                  </span>
                ) : (
                  <button
                    className={`btn primary inspection-remediate${remediationOpen ? ' active' : ''}`}
                    onClick={() => {
                      setRemediationOpen((v) => !v);
                      setRemediationError(null);
                    }}
                  >
                    <WrenchIcon size={14} /> 一键整改
                  </button>
                )}
              </div>
            </div>
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {report.markdown}
            </ReactMarkdown>
            {remediationOpen && (
              <div className="remediation-box" ref={remediationBoxRef}>
                <div className="remediation-title">
                  <WrenchIcon size={14} /> 一键整改
                </div>

                <div className="remediation-field">
                  <span className="remediation-field-label">
                    整改干预意见（可选）
                  </span>
                  <textarea
                    value={intervention}
                    onChange={(e) => setIntervention(e.target.value)}
                    placeholder="例如：只整改磁盘与日志轮转，不要改动 SSH 配置；服务重启前先通知我"
                    disabled={remediationBusy}
                  />
                </div>

                {remediation?.status !== 'plan_ready' &&
                  remediation?.status !== 'success' && (
                  <div className="remediation-actions">
                    <button
                      className="btn primary small"
                      onClick={handleGenerateRemediation}
                      disabled={remediationBusy}
                    >
                      <WrenchIcon size={13} /> 生成整改步骤
                    </button>
                  </div>
                )}

                {remediationProgress?.phase === 'planning' &&
                  remediation?.status !== 'plan_ready' && (
                  <div className="remediation-running">
                    <span className="spinner" />
                    <span>{remediationProgress.message}</span>
                  </div>
                )}

                {remediation?.status === 'plan_ready' && (
                  <>
                    {remediation.plan_markdown && (
                      <p className="remediation-summary">
                        {remediation.plan_markdown}
                      </p>
                    )}
                    <div className="remediation-steps">
                      {editSteps.map((step, index) => {
                        const dangerous = isDangerousCommand(step.command);
                        return (
                          <div className="remediation-step-edit" key={index}>
                            <div className="remediation-step-head">
                              <span className="remediation-step-index">
                                步骤 {index + 1}
                              </span>
                              {dangerous && (
                                <span className="remediation-danger-badge">
                                  危险
                                </span>
                              )}
                              <button
                                className="icon-btn danger"
                                title="删除步骤"
                                onClick={() => removeStep(index)}
                              >
                                <TrashIcon size={13} />
                              </button>
                            </div>
                            <textarea
                              className="remediation-step-desc"
                              value={step.description}
                              onChange={(e) =>
                                updateStep(index, {
                                  description: e.target.value,
                                })
                              }
                              placeholder="步骤说明"
                            />
                            <textarea
                              className="remediation-step-cmd"
                              value={step.command}
                              onChange={(e) =>
                                updateStep(index, { command: e.target.value })
                              }
                              placeholder="要执行的命令"
                              spellCheck={false}
                            />
                            <label className="remediation-step-timeout">
                              超时（秒）
                              <input
                                type="number"
                                min={5}
                                max={600}
                                value={step.timeout_secs}
                                onChange={(e) =>
                                  updateStep(index, {
                                    timeout_secs:
                                      Number.parseInt(e.target.value, 10) || 60,
                                  })
                                }
                              />
                            </label>
                          </div>
                        );
                      })}
                    </div>
                    <div className="remediation-actions">
                      <button className="btn ghost small" onClick={addStep}>
                        <PlusIcon size={13} /> 添加步骤
                      </button>
                      <button
                        className="btn secondary small"
                        onClick={handleGenerateRemediation}
                        disabled={remediationBusy}
                      >
                        <RefreshIcon size={13} /> 重新生成
                      </button>
                      <button
                        className="btn primary small"
                        onClick={handleExecute}
                        disabled={remediationBusy}
                      >
                        <CheckIcon size={13} /> 确认执行
                      </button>
                    </div>
                  </>
                )}

                {(remediation?.status === 'executing' ||
                  remediation?.status === 'success' ||
                  remediation?.status === 'failed' ||
                  remediation?.status === 'cancelled') && (
                  <div className="remediation-results">
                    {remediationProgress?.phase.startsWith('step_') && (
                      <div className="remediation-running">
                        <span className="spinner" />
                        <span>{remediationProgress.message}</span>
                      </div>
                    )}
                    {remediation.steps.map((step, index) => (
                      <div
                        className={`remediation-result-step step-${step.status}`}
                        key={step.id || index}
                      >
                        <div className="remediation-result-head">
                          <span className="remediation-step-index">
                            步骤 {index + 1}
                          </span>
                          <span className="remediation-step-state">
                            {step.status === 'pending' && '待执行'}
                            {step.status === 'running' && '执行中'}
                            {step.status === 'success' && '已完成'}
                            {step.status === 'error' && '失败'}
                          </span>
                        </div>
                        <div className="remediation-step-desc">
                          {step.description}
                        </div>
                        <code className="remediation-step-cmd-read">
                          {step.command}
                        </code>
                        {step.output != null && (
                          <pre className="remediation-step-output">
                            {step.output}
                          </pre>
                        )}
                      </div>
                    ))}
                    <div className="remediation-actions">
                      {remediation.status === 'executing' && (
                        <button
                          className="btn danger small"
                          onClick={handleCancelRemediation}
                        >
                          <StopIcon size={13} /> 取消整改
                        </button>
                      )}
                      {(remediation.status === 'failed' ||
                        remediation.status === 'cancelled') && (
                        <button
                          className="btn primary small"
                          onClick={handleRetryRemediation}
                        >
                          <RefreshIcon size={13} /> 重新执行
                        </button>
                      )}
                      {(remediation.status === 'success' ||
                        remediation.status === 'failed' ||
                        remediation.status === 'cancelled') && (
                        <button
                          className="btn secondary small"
                          onClick={() => {
                            loadRemediation(remediation.report_id).catch(() => {});
                            setRemediationOpen(false);
                          }}
                        >
                          关闭整改
                        </button>
                      )}
                    </div>
                  </div>
                )}

                {remediationError && (
                  <div className="remediation-error">{remediationError}</div>
                )}
              </div>
            )}
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
      {confirmingDanger && (
        <Modal
          title="确认执行危险整改"
          subtitle="以下步骤包含可能影响服务或数据的危险命令，请确认后执行"
          onClose={() => setConfirmingDanger(false)}
        >
          <div className="remediation-danger-modal">
            <ul>
              {pendingDangerSteps
                .filter((s) => isDangerousCommand(s.command))
                .map((step, index) => (
                  <li key={index}>
                    <code>{step.command}</code>
                  </li>
                ))}
            </ul>
            <label className="remediation-danger-check">
              <input
                type="checkbox"
                checked={dangerChecked}
                onChange={(e) => setDangerChecked(e.target.checked)}
              />
              我已了解风险，确认执行这些命令
            </label>
            <div className="remediation-actions">
              <button
                className="btn ghost small"
                onClick={() => setConfirmingDanger(false)}
              >
                取消
              </button>
              <button
                className="btn primary small"
                onClick={confirmExecuteDanger}
                disabled={!dangerChecked}
              >
                确认执行
              </button>
            </div>
          </div>
        </Modal>
      )}
    </aside>
  );
}
