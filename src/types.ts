export interface Host {
  id: string;
  name: string;
  address: string;
  port: number;
  username: string;
  auth_type: 'key' | 'password';
  key_path?: string | null;
  proxy_jump?: string | null;
  notes?: string | null;
  created_at: number;
}

export interface HostInput {
  name: string;
  address: string;
  port: number;
  username: string;
  auth_type: 'key' | 'password';
  key_path?: string;
  proxy_jump?: string;
  notes?: string;
}

export interface ImportResult {
  imported: number;
  skipped: number;
}

export interface AiProvider {
  id: string;
  name: string;
  base_url: string;
  protocol: string;
  enabled: boolean;
  created_at: number;
  models: AiModel[];
}

export interface AiModel {
  id: string;
  label: string;
  model: string;
  is_active: boolean;
}

export interface AiModelInput {
  label: string;
  model: string;
  is_active?: boolean;
}

export interface AiProviderInput {
  name: string;
  base_url: string;
  protocol?: string;
  enabled?: boolean;
  models: AiModelInput[];
  api_key?: string;
}

export interface TestResult {
  ok: boolean;
  message: string;
}

export interface AiRule {
  id: string;
  pattern: string;
  enabled: boolean;
  created_at: number;
}

export interface AuditLog {
  id: string;
  ts: number;
  session_id: number | null;
  host_id: string;
  host_label: string;
  tool_name: string;
  summary: string;
  permission_mode: string;
  approval: string;
  status: string;
  result: string | null;
  duration_ms: number | null;
}

export interface DiskInfo {
  mount: string;
  fs: string;
  total: string;
  used: string;
  percent: number;
}

export interface MemInfo {
  total_mb: number;
  used_mb: number;
  percent: number;
}

export interface TopProc {
  user: string;
  cpu: string;
  mem: string;
  cmd: string;
}

export interface MonitorSnapshot {
  ts: number;
  host_label: string;
  load: string;
  cpu_percent: number;
  mem: MemInfo;
  disks: DiskInfo[];
  top: TopProc[];
}
