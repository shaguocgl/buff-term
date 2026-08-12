import { useCallback, useEffect, useState } from 'react';
import {
  deleteInspection,
  inspectionRespond,
  listInspectionRuns,
  listInspections,
  saveInspection,
} from '../api';
import type { Host, InspectionRun } from '../types';
import Modal from './Modal';
import { PlusIcon, RadarIcon, RefreshIcon, TrashIcon } from './Icons';

interface Props {
  hosts: Host[];
  onClose: () => void;
}

const RISK_LABEL: Record<string, string> = {
  low: '低风险',
  medium: '中风险',
  high: '高风险',
};

function formatTime(ts: number) {
  return new Date(ts * 1000).toLocaleString('zh-CN', { hour12: false });
}

export default function InspectionModal({ hosts, onClose }: Props) {
  const [inspections, setInspections] = useState<
    { id: string; host_id: string; interval_min: number; enabled: boolean }[]
  >([]);
  const [runs, setRuns] = useState<InspectionRun[]>([]);
  const [hostId, setHostId] = useState('');
  const [intervalMin, setIntervalMin] = useState('60');
  const [respondingId, setRespondingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    const [ins, rs] = await Promise.all([listInspections(), listInspectionRuns(50)]);
    setInspections(ins);
    setRuns(rs);
  }, []);

  useEffect(() => {
    if (hosts.length > 0) setHostId((prev) => prev || hosts[0].id);
  }, [hosts]);

  useEffect(() => {
    load().catch((e) => setError(String(e)));
  }, [load]);

  const handleAdd = async () => {
    if (!hostId) return;
    setError(null);
    const mins = Number.parseInt(intervalMin, 10);
    if (Number.isNaN(mins) || mins < 1) {
      setError('巡检间隔必须大于 0 分钟');
      return;
    }
    try {
      await saveInspection({ host_id: hostId, interval_min: mins, enabled: true });
      setIntervalMin('60');
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const handleDelete = async (id: string) => {
    if (!window.confirm('删除这条巡检计划？')) return;
    try {
      await deleteInspection(id);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const handleRespond = async (run: InspectionRun) => {
    setRespondingId(run.id);
    setError(null);
    try {
      const text = await inspectionRespond(run.id);
      setRuns((prev) =>
        prev.map((r) => (r.id === run.id ? { ...r, respond_text: text } : r)),
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setRespondingId(null);
    }
  };

  const hostName = (id: string) =>
    hosts.find((h) => h.id === id)?.name ?? '未知主机';

  return (
    <Modal
      title="AI 定时巡检"
      subtitle="按计划自动巡检服务器安全，发现风险即时通知，可一键生成处置建议"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal">
        <div className="inspect-add">
          <label>
            主机
            <select value={hostId} onChange={(e) => setHostId(e.target.value)}>
              {hosts.map((h) => (
                <option key={h.id} value={h.id}>
                  {h.name}
                </option>
              ))}
            </select>
          </label>
          <label>
            间隔（分钟）
            <input
              value={intervalMin}
              onChange={(e) => setIntervalMin(e.target.value)}
              inputMode="numeric"
              className="input-port"
            />
          </label>
          <button className="btn primary" onClick={handleAdd}>
            <PlusIcon size={14} /> 添加巡检
          </button>
        </div>
        {error && <p className="error">{error}</p>}

        {inspections.length > 0 && (
          <div className="inspect-plans">
            {inspections.map((ins) => (
              <div key={ins.id} className="inspect-plan">
                <RadarIcon size={15} />
                <span className="inspect-plan-host">{hostName(ins.host_id)}</span>
                <span className="inspect-plan-interval">每 {ins.interval_min} 分钟</span>
                <span className={`badge ${ins.enabled ? 'badge-on' : 'badge-off'}`}>
                  {ins.enabled ? '已启用' : '已停用'}
                </span>
                <button
                  className="icon-btn danger"
                  title="删除巡检计划"
                  onClick={() => handleDelete(ins.id)}
                >
                  <TrashIcon size={14} />
                </button>
              </div>
            ))}
          </div>
        )}

        <div className="monitor-section-title">巡检记录</div>
        <div className="inspect-runs">
          {runs.length === 0 && <div className="sftp-status">暂无巡检记录</div>}
          {runs.map((run) => (
            <div key={run.id} className="inspect-run">
              <div className="inspect-run-head">
                <span className="inspect-run-time">{formatTime(run.started_at)}</span>
                <span className="inspect-run-host">{run.host_label}</span>
                <span className={`badge inspect-risk inspect-${run.risk_level}`}>
                  {RISK_LABEL[run.risk_level] ?? run.risk_level}
                </span>
                <span className={`badge inspect-status inspect-${run.status}`}>
                  {run.status === 'done' ? '已完成' : run.status === 'error' ? '出错' : '进行中'}
                </span>
              </div>
              {run.summary && <pre className="inspect-summary">{run.summary}</pre>}
              {run.respond_text && (
                <div className="inspect-respond">
                  <div className="inspect-respond-title">处置建议</div>
                  <pre className="inspect-summary">{run.respond_text}</pre>
                </div>
              )}
              {run.status === 'done' && run.risk_level !== 'low' && !run.respond_text && (
                <button
                  className="btn secondary small"
                  disabled={respondingId === run.id}
                  onClick={() => handleRespond(run)}
                >
                  <RefreshIcon size={13} />
                  {respondingId === run.id ? '生成中…' : '一键生成处置建议'}
                </button>
              )}
            </div>
          ))}
        </div>
      </div>
    </Modal>
  );
}
