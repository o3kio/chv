# WebUI API / BFF Spec

> **Status:** Executive summary. The canonical source for this material is [ADR-002-WebUI](../adr/002-webui-architecture-boundary.md). Update there first; this file is a quick orientation pointer only.

## Purpose
Provide browser-safe, session-aware, view-model-oriented APIs for the WebUI.

## Rules
- browser never calls internal node services directly
- browser never calls Cloud Hypervisor
- the backend-for-frontend (BFF) can aggregate control-plane data into UI-optimized responses
- mutations map to task-creating backend operations

## Endpoint groups
- session/auth
- overview dashboard
- nodes
- virtual machines
- volumes
- networks
- tasks
- events
- maintenance
- settings/access
- audit log (admin-only query/export; see [`component/audit-log.md`](../component/audit-log.md))

## Response style
- stable resource IDs
- explicit pagination metadata
- explicit filter metadata
- task references after every mutation
- human-readable state labels plus machine state fields

## Mutation response rule
Every mutation response must include:
- accepted/rejected state
- task id
- target resource id
- operator-visible summary
- next recommended refresh/stream path

## Streaming / live updates
MVP-1 options:
- polling first
- server-sent events later
- WebSocket optional later

Recommendation:
- polling first for simplicity
- SSE later for tasks/events if needed
