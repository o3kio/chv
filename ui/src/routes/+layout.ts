import { redirect } from '@sveltejs/kit';
import { isLoggedIn } from '#lib/api/client.ts';
import type { LayoutLoad } from './$types';

const PUBLIC_PATHS = ['/login', '/install'];

export const load: LayoutLoad = ({ url }) => {
	const isPublic = PUBLIC_PATHS.some((p) => url.pathname.startsWith(p));
	if (isPublic) return {};

	if (typeof window !== 'undefined') {
		// Browser auth is an HttpOnly cookie (#660); only the non-credential
		// hint cookie is readable here.
		if (!isLoggedIn()) {
			throw redirect(302, '/login');
		}
	}

	return {};
};
