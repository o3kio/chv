export type PageMeta = {
	page: number;
	page_size: number;
	total_items: number;
};

export type FilterMeta = {
	applied: Record<string, string>;
};

export type HealthTile = {
	key: string;
	label: string;
	status: string;
	value: string;
};

export type CapacityTile = {
	key: string;
	label: string;
	used: string;
	total: string;
};

export type RecentTask = {
	task_id: string;
	status: string;
	summary: string;
	resource_kind: string;
	resource_id: string;
	operation: string;
	started_unix_ms: number;
	/** #502: the journaled terminal-failure cause — null until a failure records it. */
	error_code?: string | null;
	error_message?: string | null;
};

export type OverviewResponse = {
	clusters_total?: number;
	clusters_healthy?: number;
	clusters_degraded?: number;
	nodes_total?: number;
	nodes_degraded?: number;
	vms_running?: number;
	vms_total?: number;
	active_tasks?: number;
	unresolved_alerts?: number;
	maintenance_nodes?: number;
	capacity_hotspots?: number;
	cpu_usage_percent?: number;
	memory_usage_percent?: number;
	storage_usage_percent?: number;
	alerts?: { summary: string; scope: string; severity: string }[];
	recent_tasks?: RecentTask[];
};

export type ListNodesRequest = {
	page: number;
	page_size: number;
	filters: Record<string, string>;
};

export type NodeListItem = {
	node_id: string;
	name: string;
	cluster: string;
	state: string;
	health: string;
	cpu: string;
	memory: string;
	storage: string;
	network: string;
	version: string;
	maintenance: boolean;
	active_tasks: number;
	alerts: number;
	hypervisor_capabilities?: string[];
};

export type ListNodesResponse = {
	items: NodeListItem[];
	page: PageMeta;
	filters: FilterMeta;
};

export type GetNodeRequest = {
	node_id: string;
};

export type RelatedTask = {
	task_id: string;
	status: string;
	summary: string;
	operation: string;
	started_unix_ms: number;
	/** #502: the journaled terminal-failure cause — null until a failure records it. */
	error_code?: string | null;
	error_message?: string | null;
};

export type NodeSummary = {
	node_id: string;
	name: string;
	cluster: string;
	state: string;
	health: string;
	version: string;
	cpu: string;
	memory: string;
	storage: string;
	network: string;
	uptime?: string;
	last_checkin?: string;
	recent_tasks: RelatedTask[];
	hypervisor_capabilities?: string[];
};

export type NodeHostedVm = {
	vm_id: string;
	name: string;
	power_state: string;
	/** #368 P3: the agent-reported failure code when power_state is 'Failed'. */
	last_error?: string;
	health: string;
	cpu: string;
	memory: string;
};

export type NodeConfigurationItem = {
	label: string;
	value: string;
};

export type NodeSection = {
	id: string;
	label: string;
	count?: number;
};

export type GetNodeResponse = {
	state: 'ready' | 'empty' | 'error';
	summary: NodeSummary;
	sections: NodeSection[];
	hostedVms: NodeHostedVm[];
	recentTasks: RelatedTask[];
	configuration: NodeConfigurationItem[];
};

export type ListVmsRequest = {
	page: number;
	page_size: number;
	filters: Record<string, string>;
};

export type VmListItem = {
	vm_id: string;
	name: string;
	node_id: string;
	power_state: string;
	/** #368 P3: the agent-reported failure code when power_state is 'Failed'. */
	last_error?: string;
	health: string;
	cpu: string;
	memory: string;
	volume_count: number;
	nic_count: number;
	last_task: string;
	alerts?: number;
};

export type ListVmsResponse = {
	items: VmListItem[];
	page: PageMeta;
	filters: FilterMeta;
};

export type GetVmRequest = {
	vm_id: string;
};

export type AttachedVolume = {
	volume_id: string;
	name: string;
	size: string;
	device_name: string;
	read_only: boolean;
	health: string;
};

export type AttachedNic = {
	nic_id: string;
	network_id: string;
	network_name: string;
	mac_address: string;
	ip_address: string;
	nic_model: string;
	addressing_mode: string;
};

export type VmEvent = {
	event_id: string;
	severity: string;
	type: string;
	summary: string;
	occurred_at: string;
	state: string;
};

export type VmSummary = {
	vm_id: string;
	name: string;
	node_id: string;
	power_state: string;
	health: string;
	cpu: string;
	memory: string;
	recent_tasks: RelatedTask[];
	attached_volumes?: AttachedVolume[];
	attached_nics?: AttachedNic[];
	snapshot_count?: number;
};

export type GetVmResponse = {
	summary: VmSummary;
};

export type HypervisorSettings = {
	cpu_nested: boolean;
	cpu_amx: boolean;
	cpu_kvm_hyperv: boolean;
	memory_mergeable: boolean;
	memory_hugepages: boolean;
	memory_shared: boolean;
	memory_prefault: boolean;
	iommu: boolean;
	rng_src: string;
	watchdog: boolean;
	landlock_enable: boolean;
	serial_mode: string;
	console_mode: string;
	pvpanic: boolean;
	tpm_type: string | null;
	tpm_socket_path: string | null;
	profile_id: string | null;
};

export type HypervisorProfile = {
	id: string;
	name: string;
	description: string | null;
	cpu_nested: boolean | null;
	cpu_amx: boolean | null;
	cpu_kvm_hyperv: boolean | null;
	memory_mergeable: boolean | null;
	memory_hugepages: boolean | null;
	memory_shared: boolean | null;
	memory_prefault: boolean | null;
	iommu: boolean | null;
	rng_src: string | null;
	watchdog: boolean | null;
	landlock_enable: boolean | null;
	serial_mode: string | null;
	console_mode: string | null;
	pvpanic: boolean | null;
	tpm_type: string | null;
	tpm_socket_path: string | null;
	is_builtin: boolean;
};

export type HypervisorSettingsResponse = {
	settings: HypervisorSettings;
	profiles: HypervisorProfile[];
};

export type HypervisorSettingsPatch = Partial<HypervisorSettings>;

export type CreateVmRequest = {
	name?: string;
	display_name?: string;
	node_id?: string;
	cpu_count?: number;
	vcpu?: number;
	memory_bytes?: number;
	memory_mb?: number;
	image_ref?: string;
	image_id?: string;
	requested_by?: string;
	network_id?: string;
	volume_size_gb?: number;
	cloud_init_userdata?: string;
	hypervisor_overrides?: Partial<HypervisorSettings>;
};

export type CreateVmResponse = {
	vm_id: string;
	operation_id: string;
	status: string;
};

export type MutateVmRequest = {
	vm_id: string;
	action: string;
	force: boolean;
	target_node_id?: string;
	pause_first?: boolean;
};

export type MutateVmResponse = {
	accepted: boolean;
	task_id: string;
	vm_id: string;
	summary: string;
};

export type GetVmConsoleRequest = {
	vm_id: string;
};

export type GetVmConsoleUrlResponse = {
	vm_id: string;
	url: string;
	/**
	 * Short-lived console ticket. Delivered separately from `url` (#660) and
	 * passed as the WebSocket subprotocol, never appended to the URL.
	 */
	token: string;
	expires_at: string;
};

export type MutateVolumeRequest = {
	volume_id: string;
	action: string;
	force: boolean;
	resize_bytes?: number;
	vm_id?: string;
};

export type MutateVolumeResponse = {
	accepted: boolean;
	task_id: string;
	volume_id: string;
	summary: string;
};

// #522 DP12 (PR 4): the delete-surface contract — exactly one key,
// mirroring the route's body (`POST /v1/volumes/delete`, #535). DP5's
// no-force stance is structural: a `force` flag exists nowhere in this
// type, so the UI cannot express it (the attached guard's 400 names
// the detach path); DP6's kind gate is server-side only, same story.
export type DeleteVolumeRequest = {
	volume_id: string;
};

export type DeleteVolumeResponse = {
	accepted: boolean;
	task_id: string;
	volume_id: string;
	summary: string;
	next_refresh_path: string;
};

// #513 DP3: the create-surface contract. `capacity_bytes` is i64 on the
// wire (1 ..= 64 TiB); `storage_class` is optional (absent/blank = the
// node's default, local). The reserved keys `attached_vm_id` and
// `seed_image_ref` are NOT part of this type — the UI must never send
// them (the BFF rejects both loudly with a 400).
export type CreateVolumeRequest = {
	name: string;
	node_id: string;
	capacity_bytes: number;
	storage_class?: string;
};

export type CreateVolumeResponse = {
	accepted: boolean;
	task_id: string;
	volume_id: string;
	summary: string;
	next_refresh_path: string;
};

export type VolumeListItem = {
	volume_id: string;
	name: string;
	node_id: string;
	health: string;
	size: string;
	attached_vm_id: string;
	attached_vm_name: string;
	status: string;
	last_task: string;
	alerts?: number;
	policy?: string;
};

export type ListVolumesRequest = {
	page: number;
	page_size: number;
	filters: Record<string, string>;
};

export type ListVolumesResponse = {
	items: VolumeListItem[];
	page: PageMeta;
	filters: FilterMeta;
};

export type GetVolumeRequest = {
	volume_id: string;
};

export type VolumeSummary = {
	volume_id: string;
	name: string;
	node_id: string;
	health: string;
	size: string;
	capacity_bytes?: number;
	status: string;
	attached_vm_id: string;
	attached_vm_name: string;
	device_name: string;
	read_only: boolean;
	volume_kind: string;
	storage_class: string;
	last_task: string;
	recent_tasks: RelatedTask[];
};

export type GetVolumeResponse = {
	summary: VolumeSummary;
};

export type CreateNodeRequest = {
	name: string;
	hostname: string;
	ip_address: string;
	agent_url?: string;
};

export type CreateNodeBffResponse = {
	id: string;
	name: string;
	hostname: string;
	ip_address: string;
	status: string;
	is_local: boolean;
	agent_url?: string;
	agent_token: string;
	created_at?: string;
};

export type MutateNodeRequest = {
	node_id: string;
	action: string;
};

export type MutateNodeResponse = {
	accepted: boolean;
	task_id: string;
	node_id: string;
	summary: string;
};

export type EnrollNodeResponse = {
	success: boolean;
	cache_cleared: boolean;
	token_ready: boolean;
	message: string;
};

export type ListTasksRequest = {
	page: number;
	page_size: number;
	filters: Record<string, string>;
};

export type TaskListItem = {
	task_id: string;
	status: string;
	operation: string;
	resource_kind: string;
	resource_id: string;
	resource_name?: string;
	actor: string;
	started_unix_ms: number;
	finished_unix_ms?: number;
	failure_summary?: string;
	/** #502: the journaled terminal-failure cause — null until a failure records it. */
	error_code?: string | null;
	error_message?: string | null;
};

export type ListTasksResponse = {
	items: TaskListItem[];
	page: PageMeta;
	filters: FilterMeta;
};

export type InfrastructureEvent = {
	event_id: string;
	severity: 'info' | 'warning' | 'critical';
	type: string;
	resource_kind: string;
	resource_id: string;
	resource_name?: string;
	summary: string;
	state: 'resolved' | 'unresolved';
	occurred_at: string;
};

export type ListEventsResponse = {
	items: InfrastructureEvent[];
	page: PageMeta;
	filters: FilterMeta;
};

export type ImportImageRequest = {
	name: string;
	source_url?: string;
	checksum?: string;
	os?: string;
	architecture?: string;
	format?: string;
};

export type ImportImageResponse = {
	image_id: string;
	name: string;
	source_url: string;
	format: string;
	status: string;
};

export type MaintenanceWindow = {
	window_id: string;
	title: string;
	status: 'active' | 'pending' | 'completed';
	started_at: string;
	expected_end_at: string;
};

export type MaintenanceNode = {
	node_id: string;
	name: string;
	status: 'draining' | 'in_maintenance' | 'ready';
	progress?: number;
};

export type GetMaintenanceResponse = {
	windows: MaintenanceWindow[];
	nodes: MaintenanceNode[];
	pending_actions: number;
	pending_operator_actions?: { id: string; summary: string }[];
	upgrade_available?: string;
	current_version?: string;
	reboot_required_nodes?: string[];
	orchestrator_health?: string;
};

// View-model types used by the UI
export type VmLifecycleAction = 'start' | 'stop' | 'restart';

export type VmLifecycleActionResult = {
	accepted: boolean;
	action: VmLifecycleAction;
	summary: string;
	taskId: string | null;
	taskLabel: string;
	taskTone: 'healthy' | 'warning' | 'degraded' | 'failed' | 'unknown';
	taskHref: string | null;
};

export type MutationActionResult = {
	accepted: boolean;
	action: string;
	summary: string;
	taskId: string | null;
	taskLabel: string;
	taskTone: 'healthy' | 'warning' | 'degraded' | 'failed' | 'unknown';
	taskHref: string | null;
};

export type IpamMode = 'internal' | 'external' | 'none';

// ─────────────────────────────────────────────────────────────────────────────
// Backup types
// ─────────────────────────────────────────────────────────────────────────────

export interface BackupJob {
	job_id: string;
	vm_id: string;
	volume_id: string | null;
	status: string;
	backup_type: string;
	target_path: string | null;
	storage_backend: string | null;
	created_at: string;
	started_at: string | null;
	completed_at: string | null;
	error_message: string | null;
	size_bytes: number | null;
}

export interface BackupHistory {
	job_id: string;
	vm_id: string;
	volume_id: string | null;
	started_at: string | null;
	completed_at: string | null;
	status: string;
	size_bytes: number | null;
	error_message: string | null;
	created_at: string;
}

export interface BackupSchedule {
	schedule_id: string;
	vm_id: string;
	volume_id: string | null;
	name: string;
	cron_expression: string;
	retention_count: number;
	backup_type: string;
	target_path: string | null;
	storage_backend: string | null;
	enabled: boolean;
	created_at: string;
	last_run_at: string | null;
	next_run_at: string | null;
}

export interface ListBackupJobsResponse {
	items: BackupJob[];
	total: number;
}

export interface ListBackupHistoryResponse {
	items: BackupHistory[];
	page: PageMeta;
	filters: FilterMeta;
}

export interface CreateBackupJobRequest {
	vm_id: string;
	volume_id?: string;
	backup_type?: string;
	target_path?: string;
	storage_backend?: string;
	schedule_id?: string;
}

export interface CreateBackupJobResponse {
	accepted: boolean;
	task_id: string;
	job_id: string;
	summary: string;
}

export interface UpdateBackupJobRequest {
	volume_id?: string;
	status?: string;
	backup_type?: string;
	target_path?: string;
	storage_backend?: string;
	started_at?: string;
	completed_at?: string;
	error_message?: string;
	size_bytes?: number;
}

export interface ExecuteBackupJobResponse {
	accepted: boolean;
	task_id: string;
	job_id: string;
	summary: string;
}

export interface CreateNetworkInput {
	name: string;
	cidr: string;
	gateway: string;
	bridge_name?: string;
	dhcp_enabled?: boolean;
	ipam_mode?: IpamMode;
	is_default?: boolean;
}

export interface UpdateNetworkInput {
	network_id: string;
	name?: string;
	cidr?: string;
	gateway?: string;
	dhcp_enabled?: boolean;
	ipam_mode?: IpamMode;
	is_default?: boolean;
}

export interface NetworkListItem {
	network_id: string;
	name: string;
	exposure: string;
	health: string;
	attached_vms: number;
	last_task: string;
	alerts: number;
	dhcp_enabled: boolean;
	ipam_mode: string;
	is_default: boolean;
}

export interface NetworkDetailModel {
	network_id: string;
	name: string;
	scope: string;
	health: string;
	exposure: string;
	policy: string;
	cidr: string;
	gateway: string;
	dhcp_enabled: boolean;
	ipam_mode: string;
	is_default: boolean;
	attached_vms: Array<{
		vm_id: string;
		display_name: string;
		runtime_status: string;
		ip_address?: string;
		mac_address?: string;
	}>;
	created_at: string;
	last_task: string;
	alerts: number;
}

export type ResizeVmRequest = {
	vm_id: string;
	cpu_count?: number;
	memory_bytes?: number;
};

export type ResizeVmResponse = {
	accepted: boolean;
	task_id: string;
	vm_id: string;
	summary: string;
};

export type DeleteImageRequest = {
	image_id: string;
};

export type DeleteImageResponse = {
	accepted: boolean;
	image_id: string;
	summary: string;
};

export type ListVmSnapshotsRequest = {
	vm_id: string;
};

export type VmSnapshotItem = {
	snapshot_id: string;
	vm_id: string;
	name: string;
	created_at: string;
	size_bytes: number;
	status: string;
};

export type ListVmSnapshotsResponse = {
	items: VmSnapshotItem[];
	page: PageMeta;
};

export type CreateSnapshotRequest = {
	vm_id: string;
	name?: string;
};

export type CreateSnapshotResponse = {
	accepted: boolean;
	task_id: string;
	snapshot_id: string;
	summary: string;
};

export type DeleteSnapshotRequest = {
	vm_id: string;
	snapshot_id: string;
};

export type DeleteSnapshotResponse = {
	accepted: boolean;
	task_id: string;
	summary: string;
};

export type RestoreSnapshotRequest = {
	vm_id: string;
	snapshot_id: string;
};

export type RestoreSnapshotResponse = {
	accepted: boolean;
	task_id: string;
	summary: string;
};

