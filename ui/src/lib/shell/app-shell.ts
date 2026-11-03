import {
	Activity,
	BellRing,
	Blocks,
	Box,
	HardDrive,
	House,
	Image,
	Network,
	Server,
	Settings,
	Wrench
} from 'lucide-svelte';

export type ShellTone = 'healthy' | 'warning' | 'degraded' | 'failed' | 'unknown';

export interface BadgeDefinition {
	label: string;
	tone: ShellTone;
}

export interface PageDefinition {
	href: string;
	navLabel: string;
	title: string;
	eyebrow: string;
	description: string;
	icon: typeof House;
	badges: BadgeDefinition[];
	aliases?: string[];
}

const pageDefinitions: PageDefinition[] = [
	{
		href: '/',
		navLabel: 'Overview',
		title: 'Overview',
		eyebrow: 'Fleet overview',
		description:
			'Fleet health, capacity pressure, active tasks, and alerts requiring attention.',
		icon: House,
		// #447 review ruling 2 (2026-10-07): the former alias path
		// `/metrics` was shadowed by the control plane's matched admin
		// prometheus route on every hard load (F5/deep link → 401 or
		// prometheus text, the SPA redirect never ran) — renamed to a
		// non-colliding top-level segment.
		aliases: ['/observability'],
		badges: [
			{ label: 'Operational', tone: 'healthy' },
			{ label: 'Task-linked', tone: 'unknown' }
		]
	},
	{
		href: '/clusters',
		navLabel: 'Clouds',
		title: 'Clouds',
		eyebrow: 'Fleet topology',
		description:
			'Cluster inventory, readiness posture, and active work across datacenters.',
		icon: Blocks,
		badges: [
			{ label: 'Operational', tone: 'healthy' },
			{ label: 'Task-linked', tone: 'unknown' }
		]
	},
	{
		href: '/nodes',
		navLabel: 'Hosts',
		title: 'Hosts',
		eyebrow: 'Compute inventory',
		description:
			'Monitor node readiness, maintenance state, version skew, and infrastructure pressure across the fleet.',
		icon: Server,
		badges: [
			{ label: 'Operational', tone: 'healthy' },
			{ label: 'Maintenance-aware', tone: 'warning' }
		]
	},
	{
		href: '/vms',
		navLabel: 'Instances',
		title: 'Instances',
		eyebrow: 'Workload operations',
		description:
			'Give operators a fast path into power state, health, placement, and the last task touching each VM.',
		icon: Box,
		badges: [
			{ label: 'Operational', tone: 'healthy' },
			{ label: 'Task-linked', tone: 'unknown' }
		]
	},
	{
		href: '/volumes',
		navLabel: 'Storage Pools',
		title: 'Storage Pools',
		eyebrow: 'Storage inventory',
		description:
			'Track attached volumes, backend class, health, capacity, and the last task that changed storage state.',
		icon: HardDrive,
		aliases: ['/storage'],
		badges: [
			{ label: 'Operational', tone: 'healthy' },
			{ label: 'Policy-aware', tone: 'unknown' }
		]
	},
	{
		href: '/networks',
		navLabel: 'Networks',
		title: 'Networks',
		eyebrow: 'Connectivity model',
		description:
			'Monitor network scope, health, public exposure, and attached workloads without surfacing low-level internals.',
		icon: Network,
		badges: [
			{ label: 'Exposure visible', tone: 'warning' },
			{ label: 'Operational', tone: 'healthy' }
		]
	},
	{
		href: '/images',
		navLabel: 'Images',
		title: 'Images',
		eyebrow: 'Provisioning inputs',
		description:
			'Manage reusable images and templates that feed VM creation without mixing them into runtime inventory.',
		icon: Image,
		aliases: ['/templates'],
		badges: [
			{ label: 'Operational', tone: 'healthy' },
			{ label: 'Template-ready', tone: 'unknown' }
		]
	},
	{
		href: '/tasks',
		navLabel: 'Tasks',
		title: 'Tasks',
		eyebrow: 'Operator work log',
		description:
			'Every mutating action should create visible task context with clear status, resource scope, and timestamps.',
		icon: Activity,
		aliases: ['/operations'],
		badges: [
			{ label: 'Operational', tone: 'healthy' },
			{ label: 'Async visible', tone: 'warning' }
		]
	},
	{
		href: '/events',
		navLabel: 'Events',
		title: 'Events',
		eyebrow: 'Incident context',
		description:
			'Surface recent events and active alerts with severity, resource scope, and acknowledgement state.',
		icon: Activity,
		badges: [
			{ label: 'Severity-aware', tone: 'warning' },
			{ label: 'Operational', tone: 'healthy' }
		]
	},
	{
		href: '/alerts',
		navLabel: 'Alerts',
		title: 'Alerts',
		eyebrow: 'Incident response',
		description:
			'Native alert rules, incident lifecycle with acknowledgment and silence, and notification delivery audit.',
		icon: BellRing,
		badges: [
			{ label: 'State-aware', tone: 'warning' },
			{ label: 'Operational', tone: 'healthy' }
		]
	},
	{
		href: '/maintenance',
		navLabel: 'Maintenance',
		title: 'Maintenance',
		eyebrow: 'Change coordination',
		description:
			'Coordinate maintenance windows, draining, and upgrade visibility without hiding the related tasks and alerts.',
		icon: Wrench,
		badges: [
			{ label: 'Change-aware', tone: 'warning' },
			{ label: 'Operational', tone: 'healthy' }
		]
	},
	{
		href: '/settings',
		navLabel: 'Settings',
		title: 'Settings',
		eyebrow: 'Operator controls',
		description:
			'Keep settings narrow, auditable, and aligned to the control-plane boundary rather than exposing backend internals.',
		icon: Settings,
		aliases: ['/quotas'],
		badges: [
			{ label: 'Auditable', tone: 'healthy' },
			{ label: 'Intentionally scoped', tone: 'unknown' }
		]
	}
];

export function getTopLevelPageDefinitions(): PageDefinition[] {
	return pageDefinitions;
}

export function getPageDefinition(pathname: string): PageDefinition {
	const matched = pageDefinitions.find(
		(page) =>
			page.href !== '/' &&
			(pathname === page.href ||
				pathname.startsWith(`${page.href}/`) ||
				page.aliases?.some(
					(alias) => pathname === alias || pathname.startsWith(`${alias}/`)
				))
	);

	return matched ?? pageDefinitions[0];
}
