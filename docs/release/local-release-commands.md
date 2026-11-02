# Local Release Commands

This document describes the commands available for building, testing, linting, and packaging CHV locally.

## Prerequisites

- Rust toolchain (pinned by `rust-toolchain.toml`; rustup resolves it automatically)
- Node.js 22.17+ + npm (for UI build)
- `nfpm` (for packaging)
- `envsubst` from gettext (for packaging)

## Makefile Commands

| Command | Description |
|---------|-------------|
| `make build` | Debug build of all workspace crates |
| `make build-release` | Release build of Rust binaries + Web UI |
| `make build-ui` | Build the Web UI only |
| `make test` | Run all workspace tests |
| `make fmt` | Format all Rust code |
| `make lint` | Run clippy linter with warnings treated as errors |
| `make check` | Run the full local release check script |
| `make package-local` | Build `.deb` and `.rpm` packages |
| `make package-deb` | Build `.deb` packages only |
| `make package-rpm` | Build `.rpm` packages only |
| `make package-smoke` | Run packaging smoke tests |
| `make package-check` | Verify built packages exist and are valid |
| `make package-safety` | Verify packaging scripts are safe and non-destructive |
| `make package-smoke-deb` | Smoke test .deb install/remove in Docker containers |
| `make package-smoke-rpm` | Smoke test .rpm install/remove in Docker containers |
| `make integration-kvm` | Run KVM integration smoke test (build + package + test) |
| `make integration-kvm-source` | Run KVM integration test using locally built binaries |
| `make integration-kvm-packages` | Run KVM integration test using existing packages |
| `make nightly` | Build nightly packages with snapshot version |
| `make publish-repo-dry-run` | Generate apt/yum repo metadata locally (dry-run) |
| `make changelog` | Extract the changelog section for the current version |
| `make release-dry-run` | Full local release pipeline without publishing |
| `make sign-checksums` | Sign SHA256SUMS with GPG or cosign if secrets configured |
| `make verify-checksums` | Verify SHA256 checksums of packages |
| `make package-lifecycle-deb` | Run full Debian package lifecycle test (install/upgrade/remove/reinstall) |
| `make package-lifecycle-rpm` | Run full RPM package lifecycle test |
| `make package-lifecycle` | Run both Debian and RPM lifecycle tests |
| `make release` | Build a release tarball (`dist/chv-<VERSION>-linux-amd64.tar.gz`) |
| `make dev-install` | Install locally built binaries to the system |
| `make bump-version` | Bump the project version (`major`, `minor`, or `patch`) |
| `make clean` | Clean build artifacts |

## Scripts

### `scripts/check-release-local.sh`

Runs the full pre-release validation suite:

1. Formatting check (`cargo fmt --all -- --check`)
2. Lint (`cargo clippy --workspace -- -D warnings`)
3. Tests (`cargo test --workspace`)
4. Release build (`cargo build --workspace --release`)
5. Version output check (`chvctl version`)
6. Hosting template copies (`cloudflare-worker.js` and
   `github-pages-index.html` must be byte-identical to `scripts/hosting/`)

Usage:
```bash
./scripts/check-release-local.sh
# or via Makefile
make check
```

### `scripts/build-release.sh`

Builds a release tarball for linux-amd64 including binaries, UI assets, systemd units, example configs, and an install script.

Output: `dist/chv-<VERSION>-linux-amd64.tar.gz`

Usage:
```bash
./scripts/build-release.sh
# or via Makefile
make release
```

### `scripts/build-packages.sh`

Builds `.deb` and `.rpm` packages using `nfpm`.

Usage:
```bash
# Build both formats
./scripts/build-packages.sh

# Skip Rust/UI build (use existing artifacts)
./scripts/build-packages.sh --skip-build

# Build only one format
./scripts/build-packages.sh --format deb
./scripts/build-packages.sh --format rpm
```

### `scripts/smoke-packages.sh`

Verifies that release binaries, UI build, and packaging configs are present and valid. Optionally builds and inspects packages if `nfpm` is available.

Usage:
```bash
./scripts/smoke-packages.sh
# or via Makefile
make package-smoke
```

### `scripts/package/check-package-files.sh`

Validates that package files were produced, contain the expected version, are non-empty, and have valid metadata.

Usage:
```bash
./scripts/package/check-package-files.sh
# or via Makefile
make package-check
```

### `scripts/package/check-safety.sh`

Audits packaging scripts and configs for safety:
- Systemd units are present and reference correct paths
- Config files are marked `config|noreplace`
- Maintainer scripts are executable
- No destructive commands (disk formatting, bridge creation, data deletion)

Usage:
```bash
./scripts/package/check-safety.sh
# or via Makefile
make package-safety
```

### `scripts/package/smoke-deb.sh`

Runs install/remove/reinstall smoke tests for `.deb` packages inside Docker containers.

Supported images (default): `debian:12`, `ubuntu:24.04`

Usage:
```bash
# Test default images with packages from dist/packages/
./scripts/package/smoke-deb.sh

# Test with a specific package directory
./scripts/package/smoke-deb.sh dist/packages

# Test custom images
IMAGES="debian:12 ubuntu:22.04" ./scripts/package/smoke-deb.sh

# or via Makefile
make package-smoke-deb
```

### `scripts/package/smoke-rpm.sh`

Runs install/remove/reinstall smoke tests for `.rpm` packages inside Docker containers.

Supported images (default): `rockylinux:9`

Usage:
```bash
# Test default images with packages from dist/packages/
./scripts/package/smoke-rpm.sh

# Test with a specific package directory
./scripts/package/smoke-rpm.sh dist/packages

# Test custom images
IMAGES="rockylinux:9 almalinux:9" ./scripts/package/smoke-rpm.sh

# or via Makefile
make package-smoke-rpm
```

### `scripts/publish/publish-repo.sh`

Generates apt and yum repository metadata from built packages and optionally uploads to a configured repository target. Runs in dry-run mode if no upload credentials are configured.

Usage:
```bash
# Dry-run (default when no secrets configured)
./scripts/publish/publish-repo.sh --packages dist/packages --channel nightly --version "$(./scripts/version.sh --deb nightly)" --dry-run

# With S3 credentials in environment
export CHV_REPO_S3_BUCKET=my-bucket
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
./scripts/publish/publish-repo.sh --packages dist/packages --channel nightly --version "$(./scripts/version.sh --deb nightly)"

# or via Makefile
make publish-repo-dry-run
```

### `scripts/release/extract-changelog.sh`

Extracts the changelog section for a specific version from `CHANGELOG.md`. Fails with a clear error if the section is missing.

Usage:
```bash
# Extract section for current VERSION
./scripts/release/extract-changelog.sh $(cat VERSION)

# Extract section for a specific version
./scripts/release/extract-changelog.sh <version>
./scripts/release/extract-changelog.sh <version>-rc.1

# or via Makefile
make changelog
```

### `scripts/release/sign-checksums.sh`

Signs `SHA256SUMS` with GPG or Cosign if signing secrets are configured. Falls back to a clear TODO message if no secrets are present.

Usage:
```bash
# Sign with GPG (requires CHV_RELEASE_GPG_KEY env var)
CHV_RELEASE_GPG_KEY="$(cat signing-key.asc)" ./scripts/release/sign-checksums.sh dist/packages/SHA256SUMS

# Sign with Cosign (requires CHV_RELEASE_COSIGN_KEY env var)
CHV_RELEASE_COSIGN_KEY="$(cat cosign.key)" ./scripts/release/sign-checksums.sh dist/packages/SHA256SUMS

# or via Makefile
make sign-checksums
```

### `scripts/package/lifecycle-deb.sh` and `lifecycle-rpm.sh`

Run full package lifecycle tests in Docker containers:
- Fresh install
- Create sentinel state (persistent data + config modifications)
- Reinstall same version
- Upgrade from old to new version
- Remove packages
- Verify persistent data and configs are preserved
- Reinstall after remove
- Verify sentinel state survives all operations

Usage:
```bash
# Debian lifecycle test
./scripts/package/lifecycle-deb.sh --new-packages dist/packages --old-packages dist/packages-old

# RPM lifecycle test
./scripts/package/lifecycle-rpm.sh --new-packages dist/packages --old-packages dist/packages-old

# Both (via Makefile)
make package-lifecycle
```

> **Note:** Upgrade tests require old packages. The Makefile automatically builds version `0.0.1` packages as the "old" version if `dist/packages-old/` does not exist.

### `scripts/integration/kvm-smoke.sh`

Runs host-level integration tests on real KVM hardware. This script requires:
- `/dev/kvm` access
- `cloud-hypervisor` binary (auto-downloaded if missing)
- Root privileges (for package install and network operations)

Test steps:
1. Host diagnostics (CPU flags, memory, disk)
2. Verify `/dev/kvm`
3. Verify/install `cloud-hypervisor`
4. Install CHV packages or use local binaries
5. Binary version checks
6. Systemd unit validation
7. Generate temporary dev TLS certs and configs
8. Start `chv-controlplane`, `chv-stord`, `chv-nwd`, `chv-agent`
9. Verify processes stay alive and ports respond
10. Collect logs
11. Cleanup (unless `--skip-cleanup`)

> **VM lifecycle testing is TODO** — pending stable enrollment, image distribution, and network bridge setup.

Usage:
```bash
# Run with built packages (default workflow)
sudo ./scripts/integration/kvm-smoke.sh --packages dist/packages

# Run with locally built binaries
sudo ./scripts/integration/kvm-smoke.sh --source

# Run with a specific binary directory
sudo ./scripts/integration/kvm-smoke.sh --binary-dir /opt/chv/bin

# Debug mode — skip cleanup
sudo ./scripts/integration/kvm-smoke.sh --packages dist/packages --skip-cleanup

# or via Makefile
make integration-kvm          # build + package + test
make integration-kvm-source   # test with local binaries
make integration-kvm-packages # test with existing packages
```

## Expected Outputs

### `make build`
```
   Compiling chv-common v<version>
   ...
    Finished `dev` profile [unoptimized + debuginfo] target(s) in Xs
```

### `make build-release`
```
==> Building Web UI...
==> Building Rust binaries (release)...
    Finished `release` profile [optimized] target(s) in Xs
```

### `make test`
```
running X tests
test result: ok. X passed; 0 failed; 0 ignored
```

### `make check`
```
===============================================
CHV Local Release Check
Version: <version>
===============================================
[1/5] Checking formatting...
  OK
[2/5] Running clippy...
  OK
[3/5] Running tests...
  OK
[4/5] Building release binaries...
  OK
[5/5] Checking CLI version output...
  Output: chvctl <version> (commit ..., build ..., channel ...)
  OK
===============================================
All checks passed!
===============================================
```

## Troubleshooting

### `cargo clippy` fails with warnings
Fix the warnings or run `cargo clippy --workspace` to see them without the `-D warnings` strictness.

### `nfpm` not found
Install with:
```bash
go install github.com/goreleaser/nfpm/v2/cmd/nfpm@latest
```

### `envsubst` not found
Install with:
```bash
apt-get install gettext   # Debian/Ubuntu
dnf install gettext       # Fedora/RHEL
```

### Tests fail with protobuf errors
Install `protoc`:
```bash
apt-get install protobuf-compiler   # Debian/Ubuntu
dnf install protobuf-compiler       # Fedora/RHEL
```

## Optional Tools

The following tools are not required but recommended for additional validation:

| Tool | Purpose | Install |
|------|---------|---------|
| `cargo-audit` | Audit dependencies for security vulnerabilities | `cargo install cargo-audit` |
| `cargo-deny` | Enforce dependency policies (licenses, advisories, bans) | `cargo install cargo-deny` |
| `cargo-nextest` | Faster test runner with better output and filtering | `cargo install cargo-nextest` |

## Service Management

After installing packages, services are present but not automatically started or enabled:

```bash
# Reload systemd to recognize new units
sudo systemctl daemon-reload

# Enable and start control plane
sudo systemctl enable --now chv-controlplane

# Enable and start node services
sudo systemctl enable --now chv-agent chv-stord chv-nwd

# Check status
sudo systemctl status chv-agent
```

> **Note:** Services require configuration (TLS certs, `jwt_secret`, `control_plane_addr`) before they can start successfully. See `/etc/chv/*.toml` for per-service config files.

## CI Parity

The local `make check` command runs the same checks as the Rust CI job in `.github/workflows/ci.yml`:
- `cargo fmt --all -- --check`
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
- Version validation (`chvctl version`)
