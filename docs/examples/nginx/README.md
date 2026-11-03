# CHV nginx Example

This directory contains a reference nginx configuration for serving the CHV Web UI and proxying API / WebSocket traffic.

## File

- **`chv-ui.conf`** — Single-server configuration for the CHV Web UI.

## Quick Start

```bash
sudo cp chv-ui.conf /etc/nginx/sites-available/chv
sudo ln -sf /etc/nginx/sites-available/chv /etc/nginx/sites-enabled/chv
sudo rm -f /etc/nginx/sites-enabled/default
sudo nginx -t
sudo systemctl restart nginx
```

## WebSocket Console Routing

The configuration supports two console access modes.

### Direct Mode

The BFF returns a full `ws://` or `wss://` URL. The browser connects directly to the hypervisor node. Enable this by setting `agent_ws_address` for each node in the CHV database.

### Proxied Mode (Default)

The BFF returns a relative path including `node_id`:
```
/ws/vms/{node_id}/{vm_id}/console
```

The console ticket is delivered separately and sent in the
`Sec-WebSocket-Protocol` header (#660), so it does not appear in the URL,
access logs, or browser history. nginx forwards the header by default.

nginx routes to the correct backend using a `map` block. Edit the `map $request_uri $ws_backend` section in `chv-ui.conf` and add one entry per node:

```nginx
map $request_uri $ws_backend {
    default              127.0.0.1:8444;
    ~^/ws/vms/node-1/   192.168.1.10:8444;
    ~^/ws/vms/node-2/   192.168.1.11:8444;
}
```

The regex extracts the node from the request URI and selects the IP:port of that node's agent console server.

## Security Notes

- Use TLS/WSS in production.
- In proxied mode, agent console ports do not need to be exposed to the browser — only nginx needs to reach them.
- For large clusters, consider OpenResty/Lua or a dedicated API gateway instead of static map entries.

## Full Documentation

See `docs/DEPLOYMENT.md` for the complete deployment guide.
