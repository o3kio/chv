# chv-monitor-agent

The optional CHV in-guest monitoring agent (ADR-026,
https://github.com/o3kio/chv — `docs/specs/adr/026-optional-monitor-agent-and-guest-identity.md`).

## What it does

Read-only guest telemetry, nothing else:

- collects CPU utilization, memory availability, one-minute load,
  uptime, and OS identity from `/proc` and `/etc/os-release`;
- enrolls once with an operator-issued claim over mutual TLS;
- delivers batches over the same TLS credential, spooling to disk
  across outages and replaying oldest-first;
- rotates its credential when the manager says so.

What it never does: mutate VM state, execute commands, enumerate
arbitrary processes or filesystems, or open inbound ports. A VM
without this agent is fully usable; installing or removing it changes
nothing about VM lifecycle.

## Enabling

1. Edit `/etc/chv-monitor/agent.toml`: set `server_url` (the manager
   base URL, always `https://`) and `manager_ca_path` (the PEM trust
   anchor for the manager's TLS certificate).
2. Ask an operator for a one-time claim (WebUI: VM detail → metrics
   tab → Guest monitoring agent → Enroll agent) and place it as root:
   `install -o chv-monitor -g chv-monitor -m 0600 <claim>
   /var/lib/chv-monitor/claim`.
3. `systemctl enable --now chv-monitor-agent`

The service is disabled by default: it refuses to start until
`server_url` and `manager_ca_path` are set, and it stays idle
(logging, not erroring) until a claim appears.

## State

`/var/lib/chv-monitor` (0700, user `chv-monitor`) holds the client
credential, the batch spool, and the per-boot sequence counters.
Deleting it while keeping the config re-randomizes the install
identity — the manager flags that as an identity conflict
(cloned-image signal) and blocks reporting until an operator clears
it. Do not put it on ephemeral storage unless that is intended.

## Removing

`apt remove chv-monitor-agent` / `dnf remove chv-monitor-agent` stops
the service and preserves state. The manager-side registration stays
until an operator revokes the agent in the WebUI.
