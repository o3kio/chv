/// <reference types="vitest/config" />
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

declare const process: { env?: Record<string, string> };

const bffProxyTarget =
	process.env?.CHV_WEBUI_BFF_PROXY_TARGET ??
	process.env?.PUBLIC_CHV_API_PROXY_TARGET ??
	'http://localhost:8888';

export default defineConfig({
	plugins: [
		tailwindcss(),
		// SvelteKit 3: project configuration lives in the sveltekit() plugin
		// options (previously svelte.config.js's config.kit). The adapter
		// options are the former svelte.config.js kit.adapter block verbatim.
		sveltekit({
			adapter: adapter({
				fallback: 'index.html',
				pages: 'build',
				assets: 'build',
				precompress: false,
				strict: false
			})
		})
	],
	resolve:
		typeof process !== 'undefined' && process.env?.VITEST
			? {
					conditions: ['browser']
				}
			: undefined,
	test: {
		environment: 'jsdom',
		include: ['src/**/*.test.ts']
	},
	server: {
		proxy: {
			'/api': {
				target: bffProxyTarget,
				changeOrigin: true
			},
			'/v1': {
				target: bffProxyTarget,
				changeOrigin: true
			},
			'/ws': {
				target: bffProxyTarget,
				changeOrigin: true,
				ws: true
			}
		}
	}
});
