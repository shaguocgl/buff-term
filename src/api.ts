import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type {
  AiProvider,
  AiProviderInput,
  AiRule,
  AlertSettings,
  AuditLog,
  HistoryEntry,
  Host,
  HostInput,
  ImportResult,
  McpApprovalRequest,
  McpRule,
  McpService,
  McpServiceInput,
  MonitorSnapshot,
  TerminalGuardApproval,
  TerminalGuardSettings,
  TerminalRule,
  TestResult,
  UpdateInfo,
  InspectionReport,
  InspectionProgressPayload,
  InspectionDonePayload,
  InspectionErrorPayload,
  Remediation,
  RemediationProgressPayload,
  RemediationDonePayload,
  RemediationErrorPayload,
  RemediationStepInput,
} from './types';

export const listHosts = () => invoke<Host[]>('list_hosts');
export const createHost = (input: HostInput) => invoke<Host>('create_host', { input });
export const updateHost = (host: Host) => invoke<void>('update_host', { host });
export const deleteHost = (id: string) => invoke<void>('delete_host', { id });
export const importSshConfig = () => invoke<ImportResult>('import_ssh_config');
export const saveHostPassword = (id: string, password: string) =>
  invoke<void>('save_host_credentials', { id, password });
export const testHostConnection = (host: Host, password?: string) =>
  invoke<TestResult>('test_host_connection', { host, password });

export const listAiProviders = () => invoke<AiProvider[]>('list_ai_providers');
export const saveAiProvider = (input: AiProviderInput, id?: string) =>
  invoke<AiProvider>('save_ai_provider', { input, id });
export const deleteAiProvider = (id: string) =>
  invoke<void>('delete_ai_provider', { id });
export const setActiveAiModel = (providerId: string, modelId: string) =>
  invoke<void>('set_active_ai_model', { providerId, modelId });
export const setActiveAiProvider = (providerId: string) =>
  invoke<void>('set_active_ai_provider', { providerId });
export const listAiRules = () => invoke<AiRule[]>('list_ai_rules');
export const addAiRule = (pattern: string) =>
  invoke<AiRule>('add_ai_rule', { pattern });
export const deleteAiRule = (id: string) =>
  invoke<void>('delete_ai_rule', { id });
export const listAuditLogs = (limit?: number) =>
  invoke<AuditLog[]>('list_audit_logs', { limit });
export const checkForUpdate = () => invoke<UpdateInfo>('check_for_update');
export const getAppVersion = () => invoke<string>('get_app_version');

export interface SftpResult {
  ok: boolean;
  text: string;
}

export const sftpList = (host: Host, path: string) =>
  invoke<SftpResult>('sftp_list', { host, path });
export const sftpDownload = (host: Host, remote: string, local: string) =>
  invoke<SftpResult>('sftp_download', { host, remote, local });
export const sftpUpload = (host: Host, local: string, remote: string) =>
  invoke<SftpResult>('sftp_upload', { host, local, remote });
export const sftpDelete = (host: Host, path: string) =>
  invoke<SftpResult>('sftp_delete', { host, path });
export const sftpMkdir = (host: Host, path: string) =>
  invoke<SftpResult>('sftp_mkdir', { host, path });
export const sftpRename = (host: Host, from: string, to: string) =>
  invoke<SftpResult>('sftp_rename', { host, from, to });

export const monitorSnapshot = (host: Host) =>
  invoke<MonitorSnapshot>('monitor_snapshot', { host });

export const startInspection = (host: Host) =>
  invoke<string>('start_inspection', { host });
export const getInspectionReport = (id: string) =>
  invoke<InspectionReport | null>('get_inspection_report', { id });
export const listInspectionReports = (hostId?: string, limit?: number) =>
  invoke<InspectionReport[]>('list_inspection_reports', {
    hostId: hostId ?? null,
    limit: limit ?? 30,
  });
export const deleteInspectionReport = (id: string) =>
  invoke<void>('delete_inspection_report', { id });
export const cancelInspection = (id: string) =>
  invoke<void>('cancel_inspection', { id });
export const onInspectionProgress = (
  cb: (payload: InspectionProgressPayload) => void,
) =>
  listen<InspectionProgressPayload>('inspection:progress', (event) =>
    cb(event.payload),
  );
export const onInspectionDone = (
  cb: (payload: InspectionDonePayload) => void,
) =>
  listen<InspectionDonePayload>('inspection:done', (event) =>
    cb(event.payload),
  );
export const onInspectionError = (
  cb: (payload: InspectionErrorPayload) => void,
) =>
  listen<InspectionErrorPayload>('inspection:error', (event) =>
    cb(event.payload),
  );

export const startRemediationPlanning = (reportId: string, intervention: string) =>
  invoke<string>('start_remediation_planning', { reportId, intervention });
export const getRemediation = (reportId: string) =>
  invoke<Remediation | null>('get_remediation', { reportId });
export const executeRemediation = (
  remediationId: string,
  steps: RemediationStepInput[],
) => invoke<void>('execute_remediation', { remediationId, steps });
export const cancelRemediation = (remediationId: string) =>
  invoke<void>('cancel_remediation', { remediationId });
export const retryRemediation = (remediationId: string) =>
  invoke<void>('retry_remediation', { remediationId });
export const onRemediationProgress = (
  cb: (payload: RemediationProgressPayload) => void,
) =>
  listen<RemediationProgressPayload>('remediation:progress', (event) =>
    cb(event.payload),
  );
export const onRemediationDone = (
  cb: (payload: RemediationDonePayload) => void,
) =>
  listen<RemediationDonePayload>('remediation:done', (event) =>
    cb(event.payload),
  );
export const onRemediationError = (
  cb: (payload: RemediationErrorPayload) => void,
) =>
  listen<RemediationErrorPayload>('remediation:error', (event) =>
    cb(event.payload),
  );

export const getAlertSettings = () =>
  invoke<AlertSettings>('get_alert_settings');
export const saveAlertSettings = (settings: AlertSettings) =>
  invoke<void>('save_alert_settings', { settings });
export const testAlertSettings = (settings: AlertSettings) =>
  invoke<TestResult>('test_alert_settings', { settings });
export const getMcpService = () => invoke<McpService>('get_mcp_service');
export const saveMcpService = (input: McpServiceInput) =>
  invoke<McpService>('save_mcp_service', { input });
export const rotateMcpToken = () =>
  invoke<McpService>('rotate_mcp_token');
export const mcpApprove = (requestId: string, allow: boolean) =>
  invoke<void>('mcp_approve', { requestId, allow });
export const listMcpRules = () => invoke<McpRule[]>('list_mcp_rules');
export const addMcpRule = (pattern: string) =>
  invoke<McpRule>('add_mcp_rule', { pattern });
export const deleteMcpRule = (id: string) =>
  invoke<void>('delete_mcp_rule', { id });

export const getTerminalGuardSettings = () =>
  invoke<TerminalGuardSettings>('get_terminal_guard_settings');
export const saveTerminalGuardSettings = (settings: TerminalGuardSettings) =>
  invoke<TerminalGuardSettings>('save_terminal_guard_settings', { settings });
export const listTerminalRules = () =>
  invoke<TerminalRule[]>('list_terminal_rules');
export const addTerminalRule = (pattern: string) =>
  invoke<TerminalRule>('add_terminal_rule', { pattern });
export const deleteTerminalRule = (id: string) =>
  invoke<void>('delete_terminal_rule', { id });
export const resetTerminalRules = () =>
  invoke<TerminalRule[]>('reset_terminal_rules');
export const sessionGuardApprove = (
  sessionId: number,
  requestId: string,
  allow: boolean,
) => invoke<void>('session_guard_approve', { sessionId, requestId, allow });

export const onTerminalGuardApproval = (
  cb: (payload: TerminalGuardApproval) => void,
) =>
  listen<TerminalGuardApproval>('terminal:guard-approval', (event) =>
    cb(event.payload),
  );
export const onMcpApprovalRequest = (
  cb: (payload: McpApprovalRequest) => void,
) => listen<McpApprovalRequest>('mcp:approval-request', (event) => cb(event.payload));
export const testAiProvider = (p: {
  base_url: string;
  model: string;
  api_key?: string;
  id?: string;
}) =>
  invoke<TestResult>('test_ai_provider', {
    baseUrl: p.base_url,
    model: p.model,
    apiKey: p.api_key,
    id: p.id,
  });

export const agentChat = (
  sessionId: number,
  message: string,
  permissionMode: 'all' | 'smart' | 'none',
) =>
  invoke<void>('agent_chat', {
    sessionId,
    message,
    permissionMode,
  });
export const agentApprove = (sessionId: number, toolCallId: string, allow: boolean) =>
  invoke<void>('agent_approve', { sessionId, toolCallId, allow });
export const agentCancel = (sessionId: number) =>
  invoke<void>('agent_cancel', { sessionId });
export const agentReset = (sessionId: number, hostId: string) =>
  invoke<void>('agent_reset', { sessionId, hostId });
export const getHistory = (hostId: string) =>
  invoke<HistoryEntry[]>('get_history', { hostId });

export interface AiStreamPayload {
  session_id: number;
  delta: string;
}

export interface AiToolPayload {
  session_id: number;
  tool_call_id: string;
  name: string;
  args: Record<string, unknown>;
  state: 'request' | 'denied' | 'running' | 'result' | 'error';
  output?: string | null;
}

export interface AiDonePayload {
  session_id: number;
}

export interface AiErrorPayload {
  session_id: number;
  message: string;
}

export const onAiStream = (
  cb: (sessionId: number, delta: string) => void,
) =>
  listen<AiStreamPayload>('ai:stream', (event) =>
    cb(event.payload.session_id, event.payload.delta),
  );

export const onAiTool = (cb: (payload: AiToolPayload) => void) =>
  listen<AiToolPayload>('ai:tool', (event) => cb(event.payload));

export const onAiDone = (cb: (sessionId: number) => void) =>
  listen<AiDonePayload>('ai:done', (event) => cb(event.payload.session_id));

export const onAiError = (cb: (sessionId: number, message: string) => void) =>
  listen<AiErrorPayload>('ai:error', (event) =>
    cb(event.payload.session_id, event.payload.message),
  );

export const openSession = (host: Host, cols: number, rows: number) =>
  invoke<number>('open_session', { host, cols, rows });
export const closeSession = (id: number) => invoke<void>('close_session', { id });
export const sessionInput = (
  id: number,
  data: number[],
  passthrough = false,
  consoleLine?: string | null,
) =>
  invoke<void>('session_input', {
    id,
    data,
    passthrough,
    consoleLine: consoleLine ?? null,
  });
export const resizeSession = (id: number, cols: number, rows: number) =>
  invoke<void>('session_resize', { id, cols, rows });

export interface TerminalDataPayload {
  session_id: number;
  data: number[];
}

export interface SessionStatusPayload {
  session_id: number;
  status: string;
}

export const onTerminalData = (
  cb: (sessionId: number, data: Uint8Array) => void,
) =>
  listen<TerminalDataPayload>('terminal:data', (event) =>
    cb(event.payload.session_id, new Uint8Array(event.payload.data)),
  );

export const onSessionStatus = (
  cb: (sessionId: number, status: string) => void,
) =>
  listen<SessionStatusPayload>('session:status', (event) =>
    cb(event.payload.session_id, event.payload.status),
  );

export interface SessionNoticePayload {
  session_id: number;
  message: string;
}

export const onSessionNotice = (
  cb: (sessionId: number, message: string) => void,
) =>
  listen<SessionNoticePayload>('session:notice', (event) =>
    cb(event.payload.session_id, event.payload.message),
  );
