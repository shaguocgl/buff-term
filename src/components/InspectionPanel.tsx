import { memo, useCallback, useEffect, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { fmtError } from '../utils/errors';
import { useI18n, type DictKey } from '../i18n';
import ConfirmModal from './ConfirmModal';
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
  /** 面板隐藏时保持挂载（巡检/整改继续运行），仅隐藏显示 */
  hidden?: boolean;
  onClose: () => void;
}

const RISK_LABEL: Record<string, DictKey> = {
  low: 'inspect.riskLow',
  medium: 'inspect.riskMedium',
  high: 'inspect.riskHigh',
  unknown: 'inspect.riskUnknown',
};

function InspectionPanel({ host, hidden = false, onClose }: Props) {
  const { t, lang } = useI18n();
  const locale = lang === 'en' ? 'en-US' : 'zh-CN';
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [status, setStatus] = useState<InspectionStatus | null>(null);
  const [progress, setProgress] =
    useState<InspectionProgressPayload | null>(null);
  const [steps, setSteps] = useState<string[]>([]);
  const [report, setReport] = useState<InspectionReport | null>(null);
  const [history, setHistory] = useState<InspectionReport[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [emailConfigured, setEmailConfigured] = useState<boolean | null>(null);
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
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
          setError(t('inspect.notFound'));
        }
      } catch (e) {
        setStatus('failed');
        setError(fmtError(e));
      }
      await refreshHistory();
    },
    [refreshHistory, t],
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
      const id = await startInspection(host.id);
      currentIdRef.current = id;
      setCurrentId(id);
    } catch (e) {
      setStatus('failed');
      setError(fmtError(e));
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

  const handleDeleteReport = (id: string) => {
    setConfirmDeleteId(id);
  };

  const doDeleteReport = async (id: string) => {
    setConfirmDeleteId(null);
    await deleteInspectionReport(id).catch((e) => {
      setError(fmtError(e));
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
      message: t('inspect.planningMsg'),
    });
    try {
      const id = await startRemediationPlanning(report.id, intervention);
      remediationIdRef.current = id;
      setRemediationProgress((prev) =>
        prev ? { ...prev, remediation_id: id } : prev,
      );
    } catch (e) {
      setRemediationBusy(false);
      setRemediationError(fmtError(e));
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
      setRemediationError(fmtError(e));
    }
  };

  const handleExecute = () => {
    const steps = editSteps.filter((s) => s.command.trim());
    if (steps.length === 0) {
      setRemediationError(t('inspect.needStep'));
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
      setRemediationError(fmtError(e));
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
    <aside
      className="inspection-panel"
      style={{
        width: 'var(--right-panel-width, 620px)',
        display: hidden ? 'none' : undefined,
      }}
    >
      <div className="inspection-header">
        <div className="inspection-title">
          <InspectIcon size={16} />
          <span>{t('inspect.title', { name: host.name })}</span>
        </div>
        <div className="inspection-actions">
          {status === 'running' && (
            <button
              className="icon-btn"
              title={t('inspect.cancelTitle')}
              onClick={handleCancel}
            >
              <StopIcon size={14} />
            </button>
          )}
          <div
            className={`inspection-mail-state${emailConfigured === false ? ' warn' : ''}`}
            title={
              emailConfigured === false
                ? t('inspect.mailMissingTitle')
                : emailConfigured === true
                  ? t('inspect.mailConfiguredTitle')
                  : t('inspect.mailCheckingTitle')
            }
          >
            {emailConfigured === false
              ? t('inspect.mailMissing')
              : emailConfigured === true
                ? t('inspect.mailConfigured')
                : t('inspect.mailChecking')}
          </div>
          <button className="icon-btn" title={t('common.close')} onClick={onClose}>
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
              {t('inspect.startTextA')}
              <strong>{host.name}</strong>
              {t('inspect.startTextB')}
            </div>
            <button className="btn primary" onClick={begin}>
              <InspectIcon size={15} /> {t('inspect.start')}
            </button>
          </div>
        )}

        {status === 'running' && (
          <div className="inspection-running">
            <div className="spinner" />
            <div>
              <div className="inspection-phase">
                {progress?.message ?? t('inspect.preparing')}
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
                  {RISK_LABEL[report.risk_level]
                    ? t(RISK_LABEL[report.risk_level])
                    : report.risk_level}
                </span>
                <span>{report.provider_name} · {report.model}</span>
                <span>
                  {report.duration_ms != null
                    ? `${(report.duration_ms / 1000).toFixed(1)}s`
                    : '—'}
                </span>
                <span>
                  {report.email_sent
                    ? t('inspect.emailSent')
                    : t('inspect.emailNotSent')}
                </span>
              </div>
              <div className="inspection-meta-actions">
                <button className="btn secondary inspection-reinspect" onClick={begin}>
                  <RefreshIcon size={14} /> {t('inspect.reinspect')}
                </button>
                {remediation?.status === 'success' ? (
                  <span className="remediation-done-badge">
                    <CheckIcon size={12} /> {t('inspect.remediated')}
                  </span>
                ) : (
                  <button
                    className={`btn primary inspection-remediate${remediationOpen ? ' active' : ''}`}
                    onClick={() => {
                      setRemediationOpen((v) => !v);
                      setRemediationError(null);
                    }}
                  >
                    <WrenchIcon size={14} /> {t('inspect.remediate')}
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
                  <WrenchIcon size={14} /> {t('inspect.remediate')}
                </div>

                <div className="remediation-field">
                  <span className="remediation-field-label">
                    {t('inspect.interventionLabel')}
                  </span>
                  <textarea
                    value={intervention}
                    onChange={(e) => setIntervention(e.target.value)}
                    placeholder={t('inspect.interventionPlaceholder')}
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
                      <WrenchIcon size={13} /> {t('inspect.generateSteps')}
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
                          <div
                            className="remediation-step-edit"
                            key={`${step.command}-${index}`}
                          >
                            <div className="remediation-step-head">
                              <span className="remediation-step-index">
                                {t('inspect.step', { n: index + 1 })}
                              </span>
                              {dangerous && (
                                <span className="remediation-danger-badge">
                                  {t('inspect.danger')}
                                </span>
                              )}
                              <button
                                className="icon-btn danger"
                                title={t('inspect.deleteStep')}
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
                              placeholder={t('inspect.stepDescPlaceholder')}
                            />
                            <textarea
                              className="remediation-step-cmd"
                              value={step.command}
                              onChange={(e) =>
                                updateStep(index, { command: e.target.value })
                              }
                              placeholder={t('inspect.stepCmdPlaceholder')}
                              spellCheck={false}
                            />
                            <label className="remediation-step-timeout">
                              {t('inspect.timeoutSecs')}
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
                        <PlusIcon size={13} /> {t('inspect.addStep')}
                      </button>
                      <button
                        className="btn secondary small"
                        onClick={handleGenerateRemediation}
                        disabled={remediationBusy}
                      >
                        <RefreshIcon size={13} /> {t('inspect.regenerate')}
                      </button>
                      <button
                        className="btn primary small"
                        onClick={handleExecute}
                        disabled={remediationBusy}
                      >
                        <CheckIcon size={13} /> {t('inspect.confirmExecute')}
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
                            {t('inspect.step', { n: index + 1 })}
                          </span>
                          <span className="remediation-step-state">
                            {step.status === 'pending' && t('inspect.statePending')}
                            {step.status === 'running' && t('inspect.stateRunning')}
                            {step.status === 'success' && t('inspect.stateSuccess')}
                            {step.status === 'error' && t('inspect.stateError')}
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
                          <StopIcon size={13} /> {t('inspect.cancelRemediation')}
                        </button>
                      )}
                      {(remediation.status === 'failed' ||
                        remediation.status === 'cancelled') && (
                        <button
                          className="btn primary small"
                          onClick={handleRetryRemediation}
                        >
                          <RefreshIcon size={13} /> {t('inspect.rerun')}
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
                          {t('inspect.closeRemediation')}
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
          <div className="inspection-history-title">{t('inspect.historyTitle')}</div>
          {history.length === 0 && (
            <div className="inspection-empty">{t('inspect.historyEmpty')}</div>
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
                {new Date(item.created_at * 1000).toLocaleString(locale)}
              </span>
              <button
                className="icon-btn danger inspection-delete"
                title={t('inspect.deleteReportTitle')}
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
          title={t('inspect.dangerTitle')}
          subtitle={t('inspect.dangerSubtitle')}
          onClose={() => setConfirmingDanger(false)}
        >
          <div className="remediation-danger-modal">
            <ul>
              {pendingDangerSteps
                .filter((s) => isDangerousCommand(s.command))
                .map((step, index) => (
                  <li key={`${step.command}-${index}`}>
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
              {t('inspect.dangerAck')}
            </label>
            <div className="remediation-actions">
              <button
                className="btn ghost small"
                onClick={() => setConfirmingDanger(false)}
              >
                {t('common.cancel')}
              </button>
              <button
                className="btn primary small"
                onClick={confirmExecuteDanger}
                disabled={!dangerChecked}
              >
                {t('inspect.confirmExecute')}
              </button>
            </div>
          </div>
        </Modal>
      )}

      {confirmDeleteId && (
        <ConfirmModal
          title={t('inspect.deleteTitle')}
          body={t('inspect.deleteBody')}
          confirmText={t('common.delete')}
          danger
          onConfirm={() => doDeleteReport(confirmDeleteId)}
          onCancel={() => setConfirmDeleteId(null)}
        />
      )}
    </aside>
  );
}

export default memo(InspectionPanel);
