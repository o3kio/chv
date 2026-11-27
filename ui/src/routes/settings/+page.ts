import type { PageLoad } from './$types';
import { getStoredToken } from '#lib/api/client.ts';
import { getSettings } from '#lib/bff/settings.ts';
import { listUsers } from '#lib/bff/users.ts';

export type SettingsModel = {
	version: string;
	build: string;
	environment: string;
	api_endpoint: string;
	session_ttl_hours: number;
	users: { id: string; email: string; role: string }[];
	usersState: 'ready' | 'error';
	state: 'ready' | 'error';
};

export const load: PageLoad = async () => {
	const token = getStoredToken() ?? undefined;

	try {
		const res = await getSettings(token);
		// The roster must reflect the control plane, never a hard-coded empty
		// list. A failed lookup is surfaced separately so it cannot be
		// mistaken for "no operators exist".
		let users: SettingsModel['users'] = [];
		let usersState: SettingsModel['usersState'] = 'ready';
		try {
			const listed = await listUsers(token);
			users = (listed.items ?? []).map((user) => ({
				id: user.user_id,
				email: user.email ?? user.username,
				role: user.role
			}));
		} catch (userErr) {
			// eslint-disable-next-line no-console
			console.error('BFF listUsers error:', userErr);
			usersState = 'error';
		}
		const model: SettingsModel = {
			version: res.version,
			build: res.build,
			environment: res.environment,
			api_endpoint: res.api_endpoint,
			session_ttl_hours: res.session_ttl_hours,
			users,
			usersState,
			state: 'ready'
		};
		return { settings: model };
	} catch (err) {
		// eslint-disable-next-line no-console
		console.error('BFF getSettings error:', err);
		const model: SettingsModel = {
			version: 'unknown',
			build: 'unknown',
			environment: 'unknown',
			api_endpoint: '/api/v1',
			session_ttl_hours: 24,
			users: [],
			usersState: 'error',
			state: 'error'
		};
		return { settings: model };
	}
};
