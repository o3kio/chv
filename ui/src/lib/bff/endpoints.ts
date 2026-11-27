/**
 * BFF HTTP endpoint mappings.
 *
 * The proto contracts (webui-bff.proto) define gRPC services. These paths
 * represent the agreed REST translation layer exposed by the BFF gateway.
 *
 * If the BFF is not yet running, set BFF_BASE_URL to a local stub or
 * leave it empty to enable client-only fallback behavior.
 */

export const BFFEndpoints = {
	overview: '/v1/overview',
	listNodes: '/v1/nodes',
	getNode: '/v1/nodes/get',
	mutateNode: '/v1/nodes/mutate',
	enrollNode: '/v1/nodes/enroll',
	listVms: '/v1/vms',
	getVm: '/v1/vms/get',
	createVm: '/v1/vms/create',
	mutateVm: '/v1/vms/mutate',
	deleteVm: '/v1/vms/delete',
	resizeVm: '/v1/vms/resize',
	listTasks: '/v1/tasks',
	listClusters: '/v1/clusters',
	listNetworks: '/v1/networks',
	getNetwork: '/v1/networks/get',
	createNetwork: '/v1/networks/create',
	updateNetwork: '/v1/networks/update',
	deleteNetwork: '/v1/networks/delete',
	listVolumes: '/v1/volumes',
	getVolume: '/v1/volumes/get',
	createVolume: '/v1/volumes/create',
	mutateVolume: '/v1/volumes/mutate',
	deleteVolume: '/v1/volumes/delete',
	listEvents: '/v1/events',
	listVmEvents: '/v1/vms/events',
	listImages: '/v1/images',
	importImage: '/v1/images/import',
	deleteImage: '/v1/images/delete',
	listVmSnapshots: '/v1/vms/snapshots',
	createSnapshot: '/v1/vms/snapshots/create',
	deleteSnapshot: '/v1/vms/snapshots/delete',
	restoreSnapshot: '/v1/vms/snapshots/restore',
	getMaintenance: '/v1/maintenance',
	getSettings: '/v1/settings',
	getHypervisorSettings: '/v1/settings/hypervisor',
	updateHypervisorSettings: '/v1/settings/hypervisor/update',
	applyHypervisorProfile: '/v1/settings/hypervisor/apply-profile',
	listHypervisorProfiles: '/v1/settings/hypervisor/profiles',

	// Backup endpoints
	listBackupJobs: '/v1/backups/jobs',
	createBackupJob: '/v1/backups/jobs',
	listBackupHistory: '/v1/backup-history',

	// Architecture Designer endpoints (Phase 0 skeleton)
	listArchitectures: '/v1/architectures/list',
	getArchitecture: '/v1/architectures/get',
	createArchitecture: '/v1/architectures/create',
	updateArchitecture: '/v1/architectures/update',
	archiveArchitecture: '/v1/architectures/archive',

	// Architecture Designer endpoints (Phase 1 — validation + YAML)
	validateArchitecture: '/v1/architectures/validate',
	validateYaml: '/v1/architectures/validate-yaml',
	generateYaml: '/v1/architectures/generate-yaml',
	importYaml: '/v1/architectures/import-yaml',

	// Architecture Designer endpoints (Phase 3 — fleet consistency check)
	architecturesCheckFleet: '/v1/architectures/check-fleet',

	// Architecture Designer endpoints (Phase 4 — plan generation)
	architecturesPlan: '/v1/architectures/plan',
	architecturesDestroyPlan: '/v1/architectures/destroy-plan',
	architecturesDiscardPlan: '/v1/architectures/discard-plan',

	// Architecture Designer endpoints (Phase 5 — apply / runs)
	architecturesApply: '/v1/architectures/apply',
	architecturesDestroy: '/v1/architectures/destroy',
	architecturesRunsList: '/v1/architectures/runs/list',

	// Architecture Designer endpoints (Phase 6 — drift detection)
	architecturesDrift: '/v1/architectures/drift',

	// Architecture Designer endpoints (NetBox projection — issue #239,
	// docs/specs/architecture-designer/contracts/netbox-api-contract.md)
	netboxConfigGet: '/v1/architectures/netbox/config/get',
	netboxConfigUpsert: '/v1/architectures/netbox/config/upsert',
	netboxConfigDelete: '/v1/architectures/netbox/config/delete',
	netboxExportDryRun: '/v1/architectures/netbox/export/dry-run',
	netboxExport: '/v1/architectures/netbox/export',
	netboxRunsList: '/v1/architectures/netbox/runs/list',
	netboxRunsGet: '/v1/architectures/netbox/runs/get',
	netboxRunsRetry: '/v1/architectures/netbox/runs/retry',

	// Native monitoring read API (query/alerts contract v1, #602).
	monitoringCatalog: '/v1/monitoring/catalog',
	monitoringOverview: '/v1/monitoring/overview',
	monitoringCurrent: '/v1/monitoring/current',
	monitoringHistory: '/v1/monitoring/history',
	monitoringChecks: '/v1/monitoring/checks',
	monitoringHealth: '/v1/monitoring/health',
	// Guest monitoring agent administration (ADR-026, #602 G3).
	monitoringAgents: '/v1/monitoring/agents',
	monitoringAgentClaim: '/v1/monitoring/agents/claim',
	monitoringAgentRevoke: '/v1/monitoring/agents/revoke',
	monitoringAgentRotate: '/v1/monitoring/agents/rotate',
	monitoringAgentReset: '/v1/monitoring/agents/reset',
	// Native alerting (query/alerts contract v1, #602 PR-6). All POST;
	// reads are viewer-gated, rule/incident mutations operator-gated.
	monitoringAlerts: '/v1/monitoring/alerts',
	monitoringAlertDetail: '/v1/monitoring/alerts/detail',
	monitoringAlertAcknowledge: '/v1/monitoring/alerts/acknowledge',
	monitoringAlertSilence: '/v1/monitoring/alerts/silence',
	monitoringAlertRules: '/v1/monitoring/alert-rules',
	monitoringAlertRuleCreate: '/v1/monitoring/alert-rules/create',
	monitoringAlertRuleUpdate: '/v1/monitoring/alert-rules/update',
	monitoringAlertRuleDelete: '/v1/monitoring/alert-rules/delete',
	monitoringAlertDeliveries: '/v1/monitoring/notifications/deliveries'
} as const;
