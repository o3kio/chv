# Left Panel Redesign Specification

> **Status (2026-10-03):** implemented and shipped, with drift. The WebUI left panel follows this specification's naming, routes, and instance action model. Two items drifted from the text below. First, the nav tree shows only instances under each host; the host-scoped Networks/Storage/Images links in section 1 did not ship in the left panel. Host-scoped filtered views exist elsewhere via `?node_id={id}` links. Second, the platform is now named CHV; this document predates that naming and retains the historical labels only in the old-label renaming tables below. Verified against `main` at `1fbb2b04` (`ui/src/lib/components/shell/NavInfrastructureTree.svelte`, `ui/src/lib/components/shell/NavGlobalLinks.svelte`, `ui/src/lib/shell/instance-actions.ts`).

## 1. Information Architecture

### Final Left Panel Hierarchy

```
CHV
Control Plane

[ Search resources... ]

INFRASTRUCTURE
▾ Default Cloud
  ▾ Hosts
    ▾ <host-name>
      ▾ Instances
        ● <instance-name>  <STATUS>
      ▾ Networks
        <network-name>
      ▾ Storage
        <pool-name>
      ▾ Images
        <image-name>

GLOBAL
  Images
  Networks
  Storage Pools
  Tasks
  Events
  Backups
  Settings
```

### Naming Rules

| Old Label | New Label | Location |
|-----------|-----------|----------|
| Default-DC | Default Cloud | Infrastructure root |
| Nodes | Hosts | Infrastructure tree |
| Virtual Machines / VMs | Instances | Infrastructure tree + global |
| Network Fabric | Networks | Global nav |
| Storage Pools | Storage Pools | Global nav (unchanged) |
| Image Library | Images | Global nav |
| Operation Pipeline | Tasks | Global nav |
| Incident Log | Events | Global nav |
| Data Protection | Backups | Global nav |
| Maintenance / Upgrades | — | Removed from left panel for MVP |

### Topology Objects vs Global Navigation

**Topology objects** (expandable tree nodes under Infrastructure):
- Cloud
- Host
- Instance
- Network (host-scoped link)
- Storage (host-scoped link)
- Image (host-scoped link)

**Global navigation entries** (flat links under GLOBAL):
- Images → `/images`
- Networks → `/networks`
- Storage Pools → `/volumes` [Corrected 2026-10-08, #514: was `/storage`; the nav was repointed when the phantom storage-pools catalog was removed — the `/storage` route now 307-redirects to `/volumes`]
- Tasks → `/tasks`
- Events → `/events`
- Backups → `/backup-jobs`
- Settings → `/settings`

### Route Mapping

| Object Type | Route Pattern |
|-------------|---------------|
| Cloud summary | `/` (fleet overview) |
| Host summary | `/nodes/{hostId}` |
| Instance summary | `/vms/{instanceId}` |
| Instance console | `/vms/{instanceId}?tab=console` |
| Global Images | `/images` |
| Global Networks | `/networks` |
| Global Storage Pools | `/volumes` [Corrected 2026-10-08, #514: was `/storage`, now a 307 redirect] |
| Global Tasks | `/tasks` |
| Global Events | `/events` |
| Global Backups | `/backup-jobs` |
| Global Settings | `/settings` |

Host-scoped Networks/Storage/Images navigate to the global page with a host filter query parameter: `/networks?node_id={hostId}`.

## 2. Interaction Model

### Selection
- Single click on a tree node selects it and navigates to its summary route.
- Selected row has a left accent marker (2px solid `--color-primary`) and a tinted background (`rgba(var(--color-primary-rgb), 0.12)`).

### Context Menu
- Right-click on an instance row opens a context menu at cursor position.
- Clicking the kebab (⋮) button on an instance row opens the same context menu.
- The menu closes on: click outside, Escape key, or selecting an action that navigates.

### Keyboard Navigation
- Arrow keys navigate expanded tree nodes (future enhancement; not required for MVP).
- Escape closes open context menus and dialogs.
- Tab navigates focusable elements inside dialogs.

### Hover Behavior
- Tree rows highlight on hover (`hover:bg-[var(--color-neutral-800)]`).
- Instance rows show a kebab button on hover/focus.

### Expand/Collapse Behavior
- Tree sections (Cloud → Hosts → Host → Instances) are individually expandable/collapsible.
- Chevron icon rotates to indicate state.
- Expansion state is local to the component (not persisted across sessions for MVP).
- Default expansion: Cloud expanded, Hosts expanded, first host expanded, Instances expanded.

## 3. Instance Operation Model

### Supported Actions

| Action | Behavior |
|--------|----------|
| Open | Navigate to instance summary (`/vms/{id}`) |
| Console | Navigate to instance console (`/vms/{id}?tab=console`) |
| Start | Call `mutateVm({ vm_id, action: 'start', force: false })` |
| Shutdown | Call `mutateVm({ vm_id, action: 'stop', force: false })` |
| Power Off | Call `mutateVm({ vm_id, action: 'stop', force: true })` |
| Restart | Call `mutateVm({ vm_id, action: 'restart', force: false })` |
| Rename | Placeholder — backend endpoint does not yet exist |
| Delete | Call `deleteVm({ vm_id, requested_by: 'webui' })` |

### State-Aware Action Rules

**RUNNING instance:**
- Open: enabled
- Console: enabled
- Start: disabled (reason: "Already running")
- Shutdown: enabled
- Power Off: enabled, dangerous
- Restart: enabled
- Rename: enabled
- Delete: enabled, dangerous

**STOPPED instance:**
- Open: enabled
- Console: disabled (reason: "Instance is stopped")
- Start: enabled
- Shutdown: disabled (reason: "Instance is stopped")
- Power Off: disabled (reason: "Instance is stopped")
- Restart: disabled (reason: "Instance is stopped")
- Rename: enabled
- Delete: enabled, dangerous

**ERROR instance:**
- Open: enabled
- Console: enabled if backend reports console available
- Start: disabled unless backend supports recovery
- Shutdown: disabled
- Power Off / Force Stop: enabled only if backend supports it
- Delete: enabled, dangerous

## 4. Delete Confirmation Behavior

- Delete must never happen directly from the context menu.
- A modal dialog is shown with:
  - Title: `Delete instance "{name}"?`
  - Body: "This permanently removes the instance configuration and selected related resources. This action cannot be undone."
  - Affected items list:
    - instance configuration
    - root disk
    - cloud-init disk
    - runtime state
  - Confirmation input: "To confirm, type the instance name."
  - Delete button disabled until typed text (trimmed) exactly matches instance name (case-sensitive).
  - Delete on Enter allowed only when button is enabled and focused.

## 5. Power Off Confirmation Behavior

- Power Off must require confirmation.
- A modal dialog is shown with:
  - Title: `Power off instance "{name}"?`
  - Body: "This is an immediate hard stop. It does not gracefully shut down the guest operating system and may cause data loss."
  - Confirm button labeled "Power Off" in warning style.
  - Cancel button present.

## 6. Visual Density Rules

- Tree row height: ~28–32px (`py-1.5` to `py-2`)
- Tree font size: 12–13px (`text-(length:--chv-text-sm)` which is 13px)
- Section labels: 10px uppercase bold (`text-[10px] font-bold uppercase`)
- Selected row: clear background tint + left accent marker
- Instance status: visible as text (e.g., "RUNNING", "STOPPED") next to the name, not color-only
- Kebab menu: exposed on hover/focus of instance rows

## 7. Backend Gaps

| Gap | Impact | Mitigation |
|-----|--------|------------|
| No rename endpoint | Rename action is always disabled | Action definition exposes `disabledReason` |
| No per-node resource list in inventory store | Networks/Storage/Images under host are links to filtered global views | Navigate with `?node_id={id}` query param |
| No explicit "error" state normalization | Error instance actions may be conservative | Treat unknown/error as stopped for action gating |

## 8. Acceptance Criteria

1. Left panel uses final naming: Cloud, Host, Instance, Network, Storage Pool, Image, Task, Event, Backup.
2. Old unclear labels removed: Default-DC, Network Fabric, Operation Pipeline, Property Mesh, Mutation Controls, Incident Log, Data Protection.
3. Infrastructure tree is topology-first (Cloud → Hosts → Host → Instances).
4. Instance rows show status in text, not only color.
5. Right-click on an instance opens a context menu.
6. Kebab menu opens the same context menu.
7. Actions are state-aware.
8. Delete is protected by typed instance-name confirmation.
9. Power Off has a warning confirmation.
10. No operational action silently succeeds without API integration.
11. Tests exist for state-aware actions and destructive confirmations.
12. UI remains visually consistent with current CHV design.
13. No fake production values introduced.
14. Code is clean, typed, and componentized.
15. Result is suitable for a serious infrastructure control plane.
