/**
 * Cloudflare Worker for hosting the CHV installer at https://get.cellhv.com/
 *
 * Setup:
 * 1. Create a Cloudflare Worker in your dashboard.
 * 2. Bind a route: get.cellhv.com/*
 * 3. Paste this code into the worker editor.
 * 4. Update GITHUB_RAW_URL to point to your install.sh on GitHub (or another CDN).
 */

const GITHUB_RAW_URL = 'https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh';

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);

    // Optional: serve a small HTML landing page for browsers
    const userAgent = request.headers.get('User-Agent') || '';
    const accept = request.headers.get('Accept') || '';
    const isBrowser = accept.includes('text/html') && !userAgent.includes('curl');

    if (isBrowser && url.pathname === '/') {
      return new Response(`<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>Install CHV</title>
  <style>
    body { font-family: system-ui, sans-serif; max-width: 720px; margin: 4rem auto; padding: 0 1rem; line-height: 1.6; }
    code { background: #f4f4f4; padding: 0.2rem 0.4rem; border-radius: 4px; }
    pre { background: #1a1a1a; color: #f4f4f4; padding: 1rem; border-radius: 8px; overflow-x: auto; }
    a { color: #0066cc; }
  </style>
</head>
<body>
  <h1>Install CHV</h1>
  <p>CHV is a Linux-first virtualization platform built on Cloud Hypervisor.</p>
  <p>Run the installer on a fresh Ubuntu server:</p>
  <pre><code>curl -fsSL https://get.cellhv.com/ | sudo bash -s</code></pre>
  <p>Or install a specific version:</p>
  <pre><code>curl -fsSL https://get.cellhv.com/ | sudo INSTALL_CHV_VERSION=0.3.0 bash -s</code></pre>
  <p>The script requires <code>bash</code> (it uses <code>set -o pipefail</code>); <code>sh -</code> does not work.</p>
  <p>See the <a href="https://github.com/o3kio/chv/blob/main/docs/DEPLOYMENT.md">deployment guide</a> for details.</p>
</body>
</html>`, {
        headers: {
          'Content-Type': 'text/html; charset=utf-8',
          'X-Content-Type-Options': 'nosniff'
        }
      });
    }

    // Fetch the install script from GitHub (or your CDN)
    const scriptResponse = await fetch(GITHUB_RAW_URL, {
      cf: { cacheTtl: 60 } // cache for 1 minute to balance freshness and origin load
    });

    if (!scriptResponse.ok) {
      return new Response('Installer script unavailable. Please try again later.', {
        status: 502,
        headers: { 'Content-Type': 'text/plain' }
      });
    }

    return new Response(scriptResponse.body, {
      headers: {
        'Content-Type': 'text/plain; charset=utf-8',
        'Cache-Control': 'public, max-age=60',
        'X-Content-Type-Options': 'nosniff'
      }
    });
  }
};
