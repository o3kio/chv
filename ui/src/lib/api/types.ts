export interface InstallStatusResponse {
  overall_state: string;
  data_root: string;
  database_path: string;
  bridge: {
    name: string;
    exists: boolean;
    expected_ip: string;
    actual_ip: string;
    up: boolean;
  };
  localdisk: {
    path: string;
    ready: boolean;
  };
  cloud_hypervisor: {
    path: string;
    found: boolean;
  };
  cloudinit: {
    supported: boolean;
  };
  checks: string[];
  warnings: string[];
  errors: string[];
}

export interface InstallActionResponse {
  overall_state: string;
  actions_taken: string[];
  warnings: string[];
  errors: string[];
}

export interface Network {
  id: string;
  name: string;
  mode: string;
  bridge_name: string;
  cidr: string;
  gateway_ip: string;
  is_system_managed: boolean;
  status: string;
  created_at: string;
}

export interface CreateNetworkInput {
  name: string;
  mode: 'bridge';
  bridge_name: string;
  cidr: string;
  gateway_ip: string;
}

export interface Image {
  id: string;
  name: string;
  os_family: string;
  architecture: string;
  format: string;
  source_url: string;
  checksum?: string;
  local_path: string;
  cloud_init_supported: boolean;
  status: string;
  created_at?: string;
}

export interface VM {
  id: string;
  name: string;
  node_id?: string;
  image_id: string;
  storage_pool_id: string;
  network_id: string;
  desired_state: string;
  actual_state: string;
  vcpu: number;
  memory_mb: number;
  disk_path: string;
  seed_iso_path: string;
  workspace_path: string;
  ip_address?: string;
  mac_address?: string;
  console_type?: 'serial';
  last_error?: string;
  user_data?: string;
  meta_data?: string;
  network_config?: string;
}

export interface CreateVMInput {
  name: string;
  image_id: string;
  storage_pool_id: string;
  network_id: string;
  vcpu: number;
  memory_mb: number;
  user_data?: string;
  username?: string;
  ssh_authorized_keys?: string[];
  console_type?: 'serial';
}

export interface Operation {
  id: string;
  resource_type: string;
  resource_id: string;
  operation_type: string;
  state: string;
  created_at: string;
}

export interface Event {
  id: string;
  timestamp: string;
  operation: string;
  status: 'pending' | 'success' | 'failed';
  resource: string;
  resource_id?: string;
  message?: string;
  details?: Record<string, any>;
}

export interface LoginResponse {
  user: {
    // The BFF login payload does not include identity/activation metadata.
    id?: string;
    username: string;
    email?: string;
    role: string;
    is_active?: boolean;
  };
  token: string;
  token_type: string;
  expires_in: number;
  must_change_password?: boolean;
}

export interface UserInfo {
  id: string;
  username: string;
  email?: string;
  role: string;
  is_active?: boolean;
}

export interface APIErrorEnvelope {
  error: {
    code: string;
    message: string;
    resource_type?: string;
    resource_id?: string;
    retryable: boolean;
    hint?: string;
  };
}

// Node types
export interface Node {
  id: string;
  name: string;
  hostname: string;
  ip_address: string;
  status: 'online' | 'offline' | 'maintenance' | 'error';
  is_local: boolean;
  agent_url?: string;
  capabilities?: string;
  last_seen_at?: string;
  created_at?: string;
  updated_at?: string;
}

export interface NodeWithResources extends Node {
  resources: {
    vms: number;
    images: number;
    storage_pools: number;
    networks: number;
  };
}

export interface CreateNodeInput {
  name: string;
  hostname: string;
  ip_address: string;
  agent_url?: string;
}

export interface CreateNodeResponse extends Node {
  agent_token: string;
}

export interface UpdateNodeInput {
  name?: string;
  hostname?: string;
  ip_address?: string;
  agent_url?: string;
}

export interface NodeMetrics {
  cpu: {
    usage_percent: number;
    cores: number;
  };
  memory: {
    total_mb: number;
    used_mb: number;
    free_mb: number;
    usage_percent: number;
  };
  storage: {
    total_bytes: number;
    used_bytes: number;
    free_bytes: number;
    usage_percent: number;
  };
}

export type ResourceType = 
  | 'vms' 
  | 'images' 
  | 'storage' 
  | 'networks' 
  | 'snapshots' 
  | 'logs' 
  | 'metrics';

// --------------------------------------------------------------------------
// Instance Resource Tree Contracts (left-panel redesign)
// --------------------------------------------------------------------------

export type InstanceStatus = 'running' | 'stopped' | 'error' | 'paused' | 'unknown';

export type InstanceActionId =
  | 'open'
  | 'console'
  | 'start'
  | 'shutdown'
  | 'poweroff'
  | 'restart'
  | 'rename'
  | 'delete';

export interface InstanceActionDefinition {
  id: InstanceActionId;
  label: string;
  enabled: boolean;
  dangerous: boolean;
  requiresConfirmation: boolean;
  disabledReason?: string;
}

export interface InstanceTreeItem {
  id: string;
  name: string;
  status: InstanceStatus;
  nodeId: string;
}

// Backup types
export interface BackupJob {
  id: string;
  job_id: string;
  vm_id: string;
  name: string;
  schedule: string;
  retention: number;
  destination?: string;
  enabled: boolean;
  created_at: string;
  updated_at?: string;
}

export interface BackupHistory {
  id: string;
  job_id: string;
  vm_id: string;
  snapshot_id: string;
  status: 'running' | 'completed' | 'failed' | 'pending';
  size_bytes: number;
  started_at?: string;
  completed_at?: string;
  error?: string;
  vm_name?: string;
}

export interface CreateBackupJobInput {
  vm_id: string;
  name: string;
  schedule: string;
  retention?: number;
  destination?: string;
}

export interface BackupJobResponse extends BackupJob {}

// Firewall Types
export interface FirewallRule {
  id: string;
  vm_id: string;
  direction: 'ingress' | 'egress';
  protocol: 'tcp' | 'udp' | 'icmp' | 'all';
  port_range?: string;
  source_cidr: string;
  action: 'allow' | 'deny';
  priority: number;
  description?: string;
  created_at: string;
}

export interface CreateFirewallRuleInput {
  direction: 'ingress' | 'egress';
  protocol: 'tcp' | 'udp' | 'icmp' | 'all';
  port_range?: string;
  source_cidr: string;
  action: 'allow' | 'deny';
  priority: number;
  description?: string;
}

// VM Template types
export interface VMTemplate {
  id: string;
  node_id: string;
  name: string;
  description?: string;
  vcpu: number;
  memory_mb: number;
  image_id: string;
  network_id: string;
  storage_pool_id: string;
  cloud_init_config?: string;
  tags?: string[];
  created_at: string;
}

export interface CreateVMTemplateInput {
  source_vm_id?: string;
  name: string;
  description?: string;
  vcpu?: number;
  memory_mb?: number;
  cloud_init_config?: string;
  tags?: string[];
}

export interface CloneFromTemplateInput {
  name: string;
  variables?: Record<string, string>;
  custom_user_data?: string;
}

// Cloud-init Template types
export interface CloudInitTemplate {
  id: string;
  name: string;
  description?: string;
  content: string;
  variables: string[];
  created_at: string;
}

export interface CreateCloudInitTemplateInput {
  name: string;
  description?: string;
  content: string;
}

export interface RenderCloudInitTemplateInput {
  variables: Record<string, string>;
}

export interface RenderCloudInitTemplateResponse {
  template_id: string;
  rendered: string;
  variables: Record<string, string>;
}

// Quota types
export interface Quota {
  id: string;
  user_id: string;
  max_vms: number;
  max_cpu: number;
  max_memory_gb: number;
  max_storage_gb: number;
  max_networks: number;
  created_at?: string;
  updated_at?: string;
}

export interface Usage {
  vms: number;
  cpus: number;
  memory_gb: number;
  storage_gb: number;
  networks: number;
}

export interface UsageWithQuota {
  quota: Quota;
  usage: Usage;
}

export interface CheckQuotaRequest {
  resource: 'vms' | 'cpu' | 'memory' | 'storage' | 'networks';
  amount: number;
}

export interface CheckQuotaResponse {
  allowed: boolean;
  resource: string;
  requested: number;
  current: number;
  limit: number;
  message?: string;
}

export interface SetQuotaInput {
  user_id: string;
  max_vms?: number;
  max_cpu?: number;
  max_memory_gb?: number;
  max_storage_gb?: number;
  max_networks?: number;
}

export interface UpdateQuotaInput {
  max_vms?: number;
  max_cpu?: number;
  max_memory_gb?: number;
  max_storage_gb?: number;
  max_networks?: number;
}
