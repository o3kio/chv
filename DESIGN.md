# Design System — CHV (Cloud Hypervisor Virtualization Platform)

## Product Context

**What this is:** A Linux-first, cloud-image-first virtualization platform for sovereign private cloud and edge cloud environments. CHV provides API-driven VM lifecycle management built on Cloud Hypervisor (the VMM).

**Who it's for:** DevOps engineers, SREs, infrastructure teams, and platform engineers. They need on-premise or edge virtualization that feels like a modern cloud provider.

**Space/industry:** Infrastructure/Virtualization — competing with VMware vSphere, Proxmox VE, OpenStack.

**Project type:** Web-based management console (dashboard) + CLI + API

---

## Aesthetic Direction

**Direction:** Warm Earthy Enterprise Console

A refined, warm-toned interface that signals "production-ready infrastructure" without the cold sterility of legacy enterprise tools. The palette draws from natural, earthy tones — warm browns, ambers, and cream. It gives the console a distinctive character while remaining entirely functional.

**Decoration level:** Minimal

No gradients, no decorative illustrations. Visual hierarchy comes from:
- Clear border definitions using warm neutral tones
- Semantic color coding for status (muted forest green / amber / terracotta)
- Typography scale with IBM Plex Sans
- Data density and structure

**Mood:** Trustworthy, functional, grounded. Infrastructure you bet your business on — with a palette that's warm rather than clinical.

---

## Typography

**Interface:** IBM Plex Sans — clean, technical, modern enterprise

**Data/Tables:** IBM Plex Mono — monospace for VM IDs, IPs, resource metrics

**Code/Terminal:** IBM Plex Mono — terminal output, logs

**Loading:** Local font install or OS-native fallback — no external CDN (see `ui/src/app.css`)

**Scale:**
| Token | Size | Pixels | Usage |
|-------|------|--------|-------|
| `--chv-text-xs` | 0.6875rem | ~11px | Labels, timestamps, metadata |
| `--chv-text-sm` | 0.8125rem | ~13px | Secondary text, captions |
| `--chv-text-base` | 0.875rem | 14px | Primary body text, table data |
| `--chv-text-lg` | 1rem | 16px | h4, section subheadings |
| `--chv-text-xl` | 1.125rem | 18px | h3, card titles |
| `--chv-text-2xl` | 1.5rem | 24px | h2, section headers |
| `--chv-text-3xl` | 1.75rem | 28px | h1, page titles |

**Weight usage:** 400 regular, 500 medium, 600 semibold, 700 bold (headings use 600)

---

## Color

**Approach:** Semantic and restrained — color carries meaning, not decoration. The palette is warm and earthy rather than the conventional cool blue/gray enterprise palette.

> **Accuracy note:** The token tables below reflect the CSS as of the
> 2026-04-19 alignment. The `ui/src/app.css` "Rust Carbon Cockpit" refactor
> (2026-04-22) changed values afterwards. The light neutral scale, the light
> shell tokens, and the radius scale no longer match `ui/src/app.css`, and
> some tokens below (motion, console, light status badges) are not defined
> there. Treat `ui/src/app.css` as the source of truth for values.

### Primary

| Role | CSS Variable | Hex | Usage |
|------|-------------|-----|-------|
| Primary | `--color-primary` | #8f5a2a | Links, buttons, active states, focus rings |
| Primary Hover | `--color-primary-hover` | #9f6837 | Button hover state |
| Primary Active | `--color-primary-active` | #76471f | Button active/pressed state |
| Primary Light | `--color-primary-light` | #f5eadc | Subtle primary tint backgrounds |
| Primary Dark | `--color-primary-dark` | #5e3513 | High-contrast primary text |

### Semantic

| Role | CSS Variable | Hex | Usage |
|------|-------------|-----|-------|
| Success | `--color-success` | #3f6b45 | Running, healthy, online |
| Success Light | `--color-success-light` | #edf4ee | Success background tint |
| Warning | `--color-warning` | #9a6a1f | Warning, degraded, maintenance |
| Warning Light | `--color-warning-light` | #f8efd9 | Warning background tint |
| Danger | `--color-danger` | #9b4338 | Stopped, error, critical |
| Danger Light | `--color-danger-light` | #faece8 | Danger background tint |
| Info | `--color-info` | #49627d | Informational states |
| Info Light | `--color-info-light` | #edf1f6 | Info background tint |

### Neutral Scale (warm cream → near-black)

| Token | Hex | Usage |
|-------|-----|-------|
| `--chv-color-neutral-50` | #f7f3ec | Page background, lightest surface |
| `--chv-color-neutral-100` | #efe9df | Sidebar background, alternate rows |
| `--chv-color-neutral-200` | #ddd5c8 | Borders, dividers |
| `--chv-color-neutral-300` | #c7bcac | Muted borders, disabled states |
| `--chv-color-neutral-400` | #9d917f | Placeholder text, scrollbar |
| `--chv-color-neutral-500` | #75695b | Secondary text, labels |
| `--chv-color-neutral-600` | #5e5449 | Muted text |
| `--chv-color-neutral-700` | #423b33 | Secondary headings |
| `--chv-color-neutral-800` | #29241f | Primary text |
| `--chv-color-neutral-900` | #191612 | Headings, highest contrast text |

### Shell / Chrome Tokens

| Token | Value | Usage |
|-------|-------|-------|
| `--shell-bg` | #f3efe7 | Application background |
| `--shell-surface` | rgba(252,249,244,0.96) | Card surfaces, dropdowns |
| `--shell-surface-muted` | rgba(247,242,234,0.96) | Hover state surfaces |
| `--shell-line` | rgba(128,112,93,0.18) | Subtle borders |
| `--shell-line-strong` | rgba(128,112,93,0.30) | Prominent borders |
| `--shell-text` | #1f262d | Body text |
| `--shell-text-secondary` | #4b5865 | Secondary/muted text |
| `--shell-accent` | #8f5a2a | Accent color (same as primary) |
| `--shell-accent-soft` | #f0dfcb | Soft accent tint |

### Status Badge Tokens

| State | Background | Border | Text |
|-------|-----------|--------|------|
| Healthy | `--status-healthy-bg` #edf4ee | rgba(63,107,69,0.18) | `--status-healthy-text` #27462d |
| Warning | `--status-warning-bg` #f8efd9 | rgba(154,106,31,0.18) | `--status-warning-text` #744d0f |
| Degraded | `--status-degraded-bg` #eef2f4 | rgba(73,98,125,0.18) | `--status-degraded-text` #304255 |
| Failed | `--status-failed-bg` #faece8 | rgba(155,67,56,0.18) | `--status-failed-text` #6e2d25 |
| Unknown | `--status-unknown-bg` #f1ede6 | rgba(117,105,91,0.18) | `--status-unknown-text` #5e5449 |

### Dark Mode

Dark mode is implemented via `[data-theme="dark"]` CSS override. Strategy: invert the neutral scale, warm up accent colors by ~10% luminance, and reduce semantic color saturation by ~15%. This avoids eye strain on dark backgrounds.

| Token | Light | Dark | Notes |
|-------|-------|------|-------|
| `--color-primary` | #8f5a2a | #c4854e | Lightened for dark bg contrast |
| `--color-primary-hover` | #9f6837 | #d49560 | |
| `--color-primary-light` | #f5eadc | #2d2318 | Inverted: dark tint surface |
| `--color-success` | #3f6b45 | #5a9e62 | Lightened |
| `--color-success-light` | #edf4ee | #1a2e1c | Dark tint surface |
| `--color-warning` | #9a6a1f | #c4922e | Lightened |
| `--color-warning-light` | #f8efd9 | #2d2410 | Dark tint surface |
| `--color-danger` | #9b4338 | #c46054 | Lightened |
| `--color-danger-light` | #faece8 | #2d1a17 | Dark tint surface |
| `--color-info` | #49627d | #6a8cad | Lightened |
| `--color-info-light` | #edf1f6 | #1a2230 | Dark tint surface |

**Dark neutral scale** (inverted, warm undertone preserved):

| Token | Light | Dark |
|-------|-------|------|
| `--chv-color-neutral-50` | #f7f3ec | #1a1816 |
| `--chv-color-neutral-100` | #efe9df | #222019 |
| `--chv-color-neutral-200` | #ddd5c8 | #2e2b24 |
| `--chv-color-neutral-300` | #c7bcac | #3d3930 |
| `--chv-color-neutral-400` | #9d917f | #6b6050 |
| `--chv-color-neutral-500` | #75695b | #9a8e7c |
| `--chv-color-neutral-600` | #5e5449 | #b5a998 |
| `--chv-color-neutral-700` | #423b33 | #d0c8ba |
| `--chv-color-neutral-800` | #29241f | #e8e2d8 |
| `--chv-color-neutral-900` | #191612 | #f5f2ec |

**Dark shell tokens:**

| Token | Dark Value |
|-------|-----------|
| `--shell-bg` | #1a1816 |
| `--shell-surface` | rgba(30, 28, 24, 0.96) |
| `--shell-surface-muted` | rgba(26, 24, 20, 0.96) |
| `--shell-line` | rgba(180, 160, 130, 0.12) |
| `--shell-line-strong` | rgba(180, 160, 130, 0.22) |
| `--shell-text` | #f0ebe3 |
| `--shell-text-secondary` | #b5a998 |
| `--shell-accent` | #c4854e |
| `--shell-accent-soft` | #2d2318 |

**Toggle:** `data-theme="dark"` attribute on `<html>` element. Persisted via `theme` Svelte store in localStorage. Defaults to system preference via `prefers-color-scheme`.

---

## Spacing

**Base unit:** 4px (0.25rem)

**Scale (CSS variables):**
| Token | Value | Pixels |
|-------|-------|--------|
| `--space-1` | 0.25rem | 4px |
| `--space-2` | 0.5rem | 8px |
| `--space-3` | 0.75rem | 12px |
| `--space-4` | 1rem | 16px |
| `--space-5` | 1.25rem | 20px |
| `--space-6` | 1.5rem | 24px |
| `--space-8` | 2rem | 32px |
| `--space-10` | 2.5rem | 40px |
| `--space-12` | 3rem | 48px |

**Density:** Comfortable-Compact — more dense than consumer SaaS, but not cramped like legacy enterprise tools.

**Layout measurements:**
- Sidebar width: `--sidebar-width` = 15.5rem (~248px)
- Header height: `--header-height` = 2.5rem (40px)
- Content max-width: `--content-max-width` = 1600px
- Card padding: `--space-4` (16px)
- Table cell padding: `--space-2` × `--space-4` (8px 16px)

---

## Layout

**Approach:** Three-pane grid-disciplined

```
┌─────────────┬──────────────────────────┬───────────────┐
│             │                          │               │
│  Sidebar    │      Content Area        │   Details     │
│  (248px)    │      (flexible)          │   (320px)     │
│             │                          │               │
└─────────────┴──────────────────────────┴───────────────┘
```

**Grid:** 12-column within content area

**Navigation:**
- Accordion sidebar with sections: Inventory, Monitoring, Administration
- Tree view for hierarchical resources (datacenters → nodes → VMs)
- Breadcrumb navigation in toolbar
- Context-sensitive actions

**Content patterns:**
- Data tables with zebra striping for resource lists
- Property grids (label | value) for object details
- Tabs for object detail views: Summary | Console | Settings | Logs
- Stats cards at top of list views

**Node-scoped navigation:**
- Datacenter overview shows aggregate stats across all nodes
- Node detail pages show node-specific resources only
- Resource tables filtered by node context
- Breadcrumbs show: Datacenter > Node Name > Resource Type

---

## Components

### Buttons

**Primary:** `--shell-accent` (#8f5a2a) background, white text, `--radius-sm` border radius
**Secondary:** `--shell-surface` background, `--shell-text` color, `--shell-line` border
**Danger:** Use `--color-danger` (#9b4338) for destructive actions

**Sizes:**
- Default: 0.4rem 0.75rem padding, `--chv-text-sm` font size
- Small (`.btn-sm`): 0.25rem 0.5rem padding, `--chv-text-xs` font size

**Transition:** `--duration-fast` (150ms) with `--ease-default`

### Data Tables

- Header: `--chv-color-neutral-100` (#efe9df) background, uppercase labels
- Rows: `--shell-surface` / `--chv-color-neutral-50` alternating
- Hover: `--shell-surface-muted`
- Selected: `--color-primary-light` (#f5eadc)
- Status column: Icon + text (not just colored text)
- Monospace font (`--font-mono`) for IDs, IPs, UUIDs

### Forms

- Labels above inputs (not inline)
- Section grouping with fieldsets
- Input borders: `--shell-line` / `--chv-color-neutral-200`
- Focus state: `--color-primary` (#8f5a2a) border, `--radius-sm`
- Font: inherits `--chv-font-sans`; size `--chv-text-sm`

### Status Indicators

| State | Icon | Color | Hex |
|-------|------|-------|-----|
| Running / Healthy | ● | Forest green | #3f6b45 |
| Stopped / Offline | ● | Neutral gray | #9d917f |
| Warning | ▲ | Warm amber | #9a6a1f |
| Error / Failed | ◼ | Terracotta red | #9b4338 |
| Degraded | ● | Steel blue | #49627d |

### Node Status

| State | Icon | Color | Description |
|-------|------|-------|-------------|
| Online | ● | #3f6b45 (forest green) | Node is healthy and responsive |
| Offline | ● | #9d917f (neutral) | Node is unreachable |
| Maintenance | 🛠 | #9a6a1f (amber) | Node is in maintenance mode |
| Error | ◼ | #9b4338 (terracotta) | Node has errors |

### Border Radius

| Token | Value | Usage |
|-------|-------|-------|
| `--radius-sm` | 0.25rem | Buttons, inputs, small chips |
| `--radius-md` | 0.5rem | Cards, panels |
| `--radius-lg` | 0.75rem | Modals, large cards |
| `--radius-xl` | 1rem | Feature panels |
| `--radius-full` | 9999px | Pills, badges |

---

## Motion

**Approach:** Minimal-functional

| Token | Value | Usage |
|-------|-------|-------|
| `--duration-instant` | 0ms | No animation |
| `--duration-fast` | 150ms | Hovers, button states |
| `--duration-normal` | 250ms | Modal transitions |
| `--duration-slow` | 350ms | Page-level transitions |

**Easing:**
- `--ease-default`: cubic-bezier(0.4, 0, 0.2, 1) — standard smooth
- `--ease-out`: cubic-bezier(0, 0, 0.2, 1) — elements entering
- `--ease-bounce`: cubic-bezier(0.34, 1.56, 0.64, 1) — playful emphasis (use sparingly)

**Rules:**
- No page load animations (instant)
- Data updates: Subtle flash on changed values
- Respects `prefers-reduced-motion` — all transitions collapse to 0.01ms

---

## Data Visualization

Colors for charts, sparklines, and metrics dashboards. The palette is derived from the semantic colors but tuned for visual distinction when multiple series appear together.

### Chart Series Colors

Use in order for multi-series charts. Each color has a 12% opacity fill variant for area charts.

| Series | Hex | CSS Variable | Fill Opacity |
|--------|-----|-------------|--------------|
| Series 1 (primary) | #8f5a2a | `var(--color-primary)` | 0.12 |
| Series 2 (info) | #49627d | `var(--color-info)` | 0.12 |
| Series 3 (success) | #3f6b45 | `var(--color-success)` | 0.12 |
| Series 4 (warning) | #9a6a1f | `var(--color-warning)` | 0.12 |
| Series 5 (accent) | #6b4f7a | `--color-chart-purple` | 0.12 |
| Series 6 (secondary) | #5e7a6b | `--color-chart-teal` | 0.12 |

### Sparkline Defaults

- Stroke: `var(--color-primary)` (#8f5a2a)
- Fill gradient: primary color at 30% opacity (top) to 5% opacity (bottom)
- Stroke width: 2px
- Bezier smoothing (cubic, control point at 1/3 intervals)
- Height: 40px inline, 120px in metric cards

### Threshold Lines

| Threshold | Color | Style |
|-----------|-------|-------|
| Warning | `var(--color-warning)` | Dashed (4px dash, 4px gap) |
| Critical | `var(--color-danger)` | Solid |
| Target/SLA | `var(--color-info)` | Dotted (2px dash, 2px gap) |

### Chart Grid and Axes

- Grid lines: `var(--shell-line)` (rgba(128, 112, 93, 0.18))
- Axis labels: `var(--shell-text-secondary)` (#4b5865), `--chv-text-xs` size
- Tooltip background: `var(--shell-surface)` with `--shadow-md`
- Tooltip border: `var(--shell-line-strong)`

---

## Serial Console / Terminal

The serial console uses a dark theme regardless of the application's light/dark mode. Terminals are always dark. All colors use CSS variables with hardcoded fallbacks.

### Console Tokens

| Token | Value | Usage |
|-------|-------|-------|
| `--console-bg` | #1a1a1a | Terminal background |
| `--console-surface` | #252525 | Toolbar background |
| `--console-line` | #333333 | Borders, dividers |
| `--console-text` | #f0ebe3 | Primary terminal text |
| `--console-text-secondary` | #aaaaaa | Status text, metadata |
| `--console-text-muted` | #888888 | Inactive labels |
| `--console-connected` | #22c55e | Connected status dot |
| `--console-disconnected` | #ef4444 | Disconnected status dot |
| `--console-error-bg` | #3b1111 | Error banner background |
| `--console-error-border` | #7f1d1d | Error banner border |
| `--console-error-text` | #fca5a5 | Error banner text |

### xterm.js Configuration

```
fontSize: 14
fontFamily: var(--font-mono)  // IBM Plex Mono
cursorBlink: true
theme: uses console tokens above
```

### Console Toolbar

- Height: matches `--header-height` (40px)
- Button size: 28px square, `--radius-sm` border radius
- Button hover: rgba(255, 255, 255, 0.08) overlay
- Status dot: 8px diameter circle
- Font size: `--chv-text-sm` (13px)

---

## Toast / Notifications

Toast notifications use the design system's semantic colors, not hardcoded values.

### Toast Variants

| Type | Background | Border-Left | Icon Color | CSS Variables |
|------|-----------|-------------|------------|---------------|
| Success | `var(--color-success-light)` | `var(--color-success)` | `var(--color-success)` | #edf4ee / #3f6b45 |
| Error | `var(--color-danger-light)` | `var(--color-danger)` | `var(--color-danger)` | #faece8 / #9b4338 |
| Info | `var(--color-info-light)` | `var(--color-info)` | `var(--color-info)` | #edf1f6 / #49627d |
| Warning | `var(--color-warning-light)` | `var(--color-warning)` | `var(--color-warning)` | #f8efd9 / #9a6a1f |

### Toast Layout

- Width: 320px max
- Padding: `--space-4` (16px)
- Border-left: 4px solid
- Shadow: `--shadow-lg`
- Border radius: `--radius-md` (8px)
- Text: `--chv-text-sm` (13px), `var(--shell-text)`
- Dismiss button: 16px icon, hover `rgba(0,0,0,0.05)` overlay
- Position: top-right, 16px from viewport edges
- Auto-dismiss: 5s for success/info, manual dismiss for error/warning
- Animation: slide in from right, `--duration-normal` (250ms), `--ease-out`

---

## RBAC UI States

CHV has three roles: `viewer` (read-only), `operator` (read+write), `admin` (read+write+admin). The UI must reflect what the current user can and cannot do.

### Permission-Gated UI Patterns

| Element | Viewer | Operator | Admin |
|---------|--------|----------|-------|
| Data tables | Full read access | Full read access | Full read access |
| Create buttons | Hidden | Visible | Visible |
| Delete buttons | Hidden | Visible | Visible |
| Mutation actions (start/stop/restart) | Hidden | Visible | Visible |
| Settings pages | Read-only view | Read-only view | Full access |
| User management (`/settings/users`) | Redirected to `/settings` | Redirected to `/settings` | Full access |
| API tokens page | Own tokens only | Own tokens only | All tokens |

### Visual Treatment for Disabled/Gated Elements

- **Hidden approach (default):** Don't render the button/action at all. Viewers see a clean read-only interface, not a field of disabled buttons.
- **Disabled approach (when context helps):** Show the element at 50% opacity with `cursor: not-allowed` and a tooltip: "Requires operator or admin role". Use this only when hiding the element would make the interface confusing (e.g., a settings form where some fields are editable and some aren't).
- **Role badge:** Show current role in the user menu. Format: pill badge using `--radius-full`, `--chv-text-xs`, uppercase. Colors: viewer=`--chv-color-neutral-400`, operator=`--color-info`, admin=`--color-primary`.

### Server-Side Enforcement

Role checks happen server-side (BearerToken + require_operator_or_admin / require_admin). The UI patterns above are progressive enhancement. A viewer who bypasses the UI still gets a 403 from the API.

---

## New Component Patterns (Sprint 8-10)

Components that shipped after the original design system was written. Document their patterns here for consistency.

### Snapshot Panel

- Container: `SectionCard` with `--radius-md` border
- Snapshot rows: property grid layout (label | value)
- Actions: inline icon buttons (restore, delete) at row end
- Restore requires VM stopped (disabled + tooltip if running)
- Delete shows `ConfirmDialog` before proceeding

### Firewall Rules Table

- Standard `InventoryTable` component
- CIDR values in `--font-mono`
- Protocol/port in `--font-mono`
- Action column: Allow (success badge) / Deny (danger badge)

### Storage Pool Cards

- Grid layout: 3 columns on desktop, 1 on mobile
- Capacity bar: full width, `--radius-sm`, height 8px
- Bar colors: `--color-success` (used), `--chv-color-neutral-200` (free), `--color-danger` when >90% used
- Pool name: `--chv-text-lg`, `font-weight: 600`
- Stats: `--font-mono` for sizes (GiB, TiB)

### User Management Table

- Admin-only page (server-side gate + client redirect)
- Role column: colored pill badges (see RBAC section)
- Created date: relative time format ("3 days ago")
- Actions: edit role dropdown, delete with confirmation

### API Token Display

- Token shown once on creation in a `--font-mono` read-only input
- "Copy" button with check icon feedback (2s timeout)
- Token list shows SHA-256 prefix (first 8 chars) in `--font-mono`
- Expiry: relative time, warning color if <7 days remaining

### Cloud-Init Editor

- Textarea with `--font-mono` font
- Syntax highlighting: none (plain text YAML)
- Min height: 200px, resizable vertically
- Border: `--shell-line`, focus: `--color-primary` border

---

## Reference Implementations

**Primary inspiration:**
- VMware vSphere HTML5 Client (layout patterns, data density)
- Proxmox VE Web Interface (tree navigation, resource management)
- Windows Admin Center (three-pane layout)

**Anti-patterns to avoid:**
- AWS Console (too cluttered, information overload)
- Vercel Dashboard (too modern/spacious for infrastructure tooling)
- Generic SaaS dashboard aesthetics (cold blue palette, excessive whitespace)
- Excessive use of bright/saturated status colors — use the muted semantic palette

---

## Decisions Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2024-04-05 | VMware/Proxmox style chosen | User explicitly requested this direction for enterprise credibility |
| 2024-04-05 | Light mode as default | Matches enterprise virtualization console conventions |
| 2024-04-05 | Three-pane layout | Industry standard for resource management since Windows Explorer |
| 2024-04-05 | Border-heavy UI | Creates clear separation between functional areas |
| 2026-04-10 | Node-scoped navigation | Multi-node support requires clear resource ownership in the UI |
| 2026-04-10 | Tree navigation with node hierarchy | Proxmox-style datacenter → node → resource drill-down pattern |
| 2026-04-19 | Warm earthy palette (#8f5a2a primary, cream neutrals) replaces cool blue (#0066CC) | CSS implementation diverged from original spec; aligning docs to match actual implementation |
| 2026-04-19 | IBM Plex Sans / IBM Plex Mono replaces Roboto | CSS implementation uses IBM Plex family; docs aligned to match |
| 2026-04-19 | Muted semantic colors (forest green, amber, terracotta) replace saturated originals | Earthy palette requires desaturated status colors for harmony |
| 2026-04-20 | Dark mode tokens documented | CSS implementation existed since sprint 6, DESIGN.md now aligned |
| 2026-04-20 | Data visualization color system added | Charts need consistent multi-series palette derived from semantic colors |
| 2026-04-20 | Serial console terminal tokens added | Console always uses dark theme regardless of app mode |
| 2026-04-20 | Toast notification spec updated | Migrate from hardcoded colors to design system semantic variables |
| 2026-04-20 | RBAC UI state patterns documented | Three-role system (viewer/operator/admin) needs clear UI guidelines |
| 2026-04-20 | Sprint 8-10 component patterns documented | Snapshots, firewall, storage, users, API tokens, cloud-init |
| 2026-10-03 | Accuracy pass: preview section removed, Toast note corrected, token-drift note added | `docs/design-preview.html` no longer exists; `Toast.svelte` uses semantic variables; `ui/src/app.css` changed after the 2026-04-19 alignment |

---

## Usage Guidelines

**Always read this file before making visual or UI decisions.**

All font choices, colors, spacing, and aesthetic direction are defined here.
Do not deviate without explicit user approval.

In QA mode, flag any code that doesn't match this design system.
