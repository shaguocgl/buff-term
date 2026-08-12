import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type {
  AiProvider,
  AiProviderInput,
  AiRule,
  AlertInput,
  AlertRule,
  AuditLog,
  Host,
  HostInput,
  Inspection,
  InspectionInput,
  InspectionRun,
  ImportResult,
  McpServer,
  McpServerInput,
  McpToolInfo,
  MonitorSnapshot,
  TestResult,
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

export const listAlerts = () => invoke<AlertRule[]>('list_alerts');
export const saveAlert = (input: AlertInput, id?: string) =>
  invoke<AlertRule>('save_alert', { input, id });
export const deleteAlert = (id: string) =>
  invoke<void>('delete_alert', { id });

export const listInspections = () => invoke<Inspection[]>('list_inspections');
export const saveInspection = (input: InspectionInput, id?: string) =>
  invoke<Inspection>('save_inspection', { input, id });
export const deleteInspection = (id: string) =>
  invoke<void>('delete_inspection', { id });
export const listInspectionRuns = (limit?: number) =>
  invoke<InspectionRun[]>('list_inspection_runs', { limit });
export const inspectionRespond = (runId: string) =>
  invoke<string>('inspection_respond', { runId });

export const listMcpServers = () => invoke<McpServer[]>('list_mcp_servers');
export const saveMcpServer = (input: McpServerInput, id?: string) =>
  invoke<McpServer>('save_mcp_server', { input, id });
export const deleteMcpServer = (id: string) =>
  invoke<void>('delete_mcp_server', { id });
export const mcpTest = (server: McpServer) =>
  invoke<McpToolInfo[]>('mcp_test', { server });
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
export const agentReset = (sessionId: number) =>
  invoke<void>('agent_reset', { sessionId });

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
export const sessionInput = (id: number, data: number[]) =>
  invoke<void>('session_input', { id, data });
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
