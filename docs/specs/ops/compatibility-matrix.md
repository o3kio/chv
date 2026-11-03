# Compatibility Matrix

## Purpose
Track supported version combinations for a tested node bundle.

Related contracts:

- [Authority-mode feature matrix](mode-feature-matrix.md) — what `legacy`,
  `core-managed`, and `core-native` each compose, and the legacy removal
  target.
- [BFF HTTP API versioning](bff-http-api-versioning.md) — the `/v1` contract,
  additive vs breaking changes, and the deprecation policy.

## Required dimensions
- control plane version
- `chv-agent` version
- Volvisor API and daemon version for bundles that integrate it
- `chv-stord` version
- `chv-nwd` version
- Cloud Hypervisor version
- host helper bundle version

## Rules
- each supported node bundle must be testable as a versioned unit
- independent component upgrade is allowed only when this matrix explicitly permits it
- rollback targets must also exist in this matrix

## Suggested table shape

| Control Plane | chv-agent | Volvisor API/daemon | chv-stord | chv-nwd | Cloud Hypervisor | Host Bundle | Status | Notes |
|---|---|---|---|---|---|---|---|---|
| 0.1.x | 0.1.x | — | 0.1.x | 0.1.x | pinned | 0.1.x | current | Legacy storage implementation |
| TBD | TBD | pinned pair | transition only | TBD | pinned | TBD | planned | Volvisor integration; qualify before support |

## Versioning guidance
- use explicit compatibility ranges where possible
- do not allow silent semantic drift across RPC contracts
- bump API version namespaces when backward compatibility breaks
