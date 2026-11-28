//! Fail-closed startup authority selection for the NodeCache-to-Core cutover.
//!
//! Wired into `cmd/chv-agent`'s core-managed startup (`start_core_managed`)
//! and the runtime-owner validation path: this library decides which
//! persistence authority a startup may activate and performs only durable
//! migration bookkeeping; it has no VM or provider side effects. For a
//! healthy, host-bearing Core authority the NodeCache beside the database is
//! the live compatibility projection and its content never gates activation;
//! fail-closed behavior is reserved for authority selection itself (identity,
//! checksums, archives, pristine-import targets).

mod identity;

pub use identity::{
    create_fresh_authority, resolve_host_identity, resolve_host_identity_with, FreshHostIdentity,
    FreshIdentitySource, HostIdentityDecision, HostIdentityError, HostIdentityInputs,
};

use cellhv_core_operations::{MigrationDisposition, OperationService, OperationServiceError};
use cellhv_nodecache_migration::{plan, MigrationError, SOURCE_NAME};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct StartupPaths {
    pub node_cache: PathBuf,
    pub core_database: PathBuf,
    pub node_cache_archive: PathBuf,
}

/// Evidence describing how an [`ActivatedStore`] became authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationKind {
    Existing,
    ImportedNodeCache,
    Fresh,
}

/// A lock-held startup snapshot used only for durable database activation.
pub struct StartupTransaction {
    paths: StartupPaths,
    cache: Option<Vec<u8>>,
    database_exists: bool,
    runtime_lease: cellhv_core_fs::RuntimeAuthorityLease,
    authority_lock: cellhv_core_fs::AuthorityLock,
}

/// Opaque proof that this process still owns the Core database runtime lease.
pub struct RuntimeAuthorityGuard {
    _lease: cellhv_core_fs::RuntimeAuthorityLease,
}

/// Validated provenance for the compatibility snapshot used during activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationProvenance {
    source_checksum: Option<String>,
    live_cache_present: bool,
    any_migration_state: bool,
    foreign_migration_state: bool,
}

impl ActivationProvenance {
    pub fn source_checksum(&self) -> Option<&str> {
        self.source_checksum.as_deref()
    }

    pub fn live_cache_present(&self) -> bool {
        self.live_cache_present
    }

    pub fn has_any_migration_state(&self) -> bool {
        self.any_migration_state
    }

    /// Whether the authority carries migration state from a source other
    /// than the NodeCache cutover (`SOURCE_NAME`): an unknown importer
    /// produced this authority. Disqualifying for every composition — the
    /// NodeCache cutover markers themselves are expected only where the
    /// mode's design says so.
    pub fn has_foreign_migration_state(&self) -> bool {
        self.foreign_migration_state
    }
}

/// An already-open Core store with process-lifetime database exclusion held.
/// This does not select or authorize a NodeCache compatibility mode.
pub struct ActivatedStore {
    service: OperationService,
    kind: ActivationKind,
    runtime_guard: RuntimeAuthorityGuard,
    provenance: ActivationProvenance,
    /// Set only by [`StartupTransaction::activate_native_only`]: the
    /// activation was requested by the native-only composition (no legacy
    /// surface), whose runtime owner fences execution against migrated or
    /// cache-adjacent state. Managed activations leave this false: the
    /// core-managed composition is designed to boot from an imported legacy
    /// NodeCache (cutover) and to run beside the live compatibility
    /// projection cache (M2.2b), so those provenance signals are expected
    /// there rather than disqualifying.
    native_only: bool,
}

/// Database activation completed while the exact NodeCache snapshot and its
/// short transaction lock are still retained for agent composition.
pub struct PendingActivatedStore {
    activated: ActivatedStore,
    node_cache_path: PathBuf,
    cache: Option<Vec<u8>>,
    authority_lock: cellhv_core_fs::AuthorityLock,
}

impl PendingActivatedStore {
    pub fn cache_bytes(&self) -> Option<&[u8]> {
        self.cache.as_deref()
    }

    pub fn node_cache_path(&self) -> &Path {
        &self.node_cache_path
    }

    pub fn provenance(&self) -> &ActivationProvenance {
        self.activated.provenance()
    }

    pub fn finish(self) -> ActivatedStore {
        let Self {
            activated,
            authority_lock,
            ..
        } = self;
        drop(authority_lock);
        activated
    }
}

impl ActivatedStore {
    pub fn service(&self) -> &OperationService {
        &self.service
    }

    pub fn service_mut(&mut self) -> &mut OperationService {
        &mut self.service
    }

    pub fn kind(&self) -> ActivationKind {
        self.kind
    }

    pub fn provenance(&self) -> &ActivationProvenance {
        &self.provenance
    }

    /// Whether this activation was requested by the native-only composition
    /// ([`StartupTransaction::activate_native_only`]). See the field
    /// documentation on [`ActivatedStore`].
    pub fn native_only(&self) -> bool {
        self.native_only
    }

    pub fn into_runtime_parts(
        self,
    ) -> (
        OperationService,
        ActivationKind,
        RuntimeAuthorityGuard,
        ActivationProvenance,
    ) {
        (self.service, self.kind, self.runtime_guard, self.provenance)
    }
}

impl StartupTransaction {
    pub fn activate_native_only(self, configured_seed: Option<String>) -> Result<ActivatedStore> {
        if self.cache.is_some() {
            return Err(StartupError::LegacyCachePresent);
        }
        let mut activated = self.activate(configured_seed, None)?;
        activated.native_only = true;
        Ok(activated)
    }

    /// Acquires process-lifetime exclusion and the NodeCache transaction lock,
    /// then snapshots both persistence sources while both remain held.
    pub fn begin(paths: &StartupPaths) -> Result<Self> {
        validate_paths(paths)?;
        let runtime_lease = cellhv_core_fs::RuntimeAuthorityLease::acquire(&paths.core_database)
            .map_err(|source| io_error(&paths.core_database, source))?;
        let authority_lock = cellhv_core_fs::AuthorityLock::acquire(&paths.node_cache)
            .map_err(|source| io_error(&paths.node_cache, source))?;
        let cache = read_optional(&paths.node_cache)?;
        let database_exists = paths
            .core_database
            .try_exists()
            .map_err(|source| io_error(&paths.core_database, source))?;
        Ok(Self {
            paths: paths.clone(),
            cache,
            database_exists,
            runtime_lease,
            authority_lock,
        })
    }

    /// Resolves identity, completes import/fresh/open, releases the short cache
    /// lock, and transfers only process-lifetime exclusion into the result.
    pub fn activate(
        self,
        configured_seed: Option<String>,
        precreation_enrollment: Option<String>,
    ) -> Result<ActivatedStore> {
        Ok(self
            .prepare_activation(configured_seed, precreation_enrollment)?
            .finish())
    }

    pub fn prepare_activation(
        self,
        configured_seed: Option<String>,
        precreation_enrollment: Option<String>,
    ) -> Result<PendingActivatedStore> {
        let Self {
            paths,
            cache,
            database_exists,
            runtime_lease,
            authority_lock,
        } = self;

        let live_cache_present = cache.is_some();
        let retained_cache = cache.clone();
        let (service, kind, source_checksum) = match (cache, database_exists) {
            (None, false) => {
                let decision = resolve_host_identity(HostIdentityInputs {
                    configured_seed,
                    precreation_enrollment,
                    ..HostIdentityInputs::default()
                })?;
                (
                    create_fresh_authority(&paths.core_database, &decision)?,
                    ActivationKind::Fresh,
                    None,
                )
            }
            (Some(bytes), false) => {
                let import = plan(&bytes)?;
                resolve_host_identity(HostIdentityInputs {
                    importable_nodecache: Some(import.host().clone()),
                    configured_seed,
                    precreation_enrollment,
                    ..HostIdentityInputs::default()
                })?;
                archive_exact(&paths.node_cache_archive, &bytes, &mut |_| Ok(()))?;
                let mut service = OperationService::create_migration_target(&paths.core_database)?;
                set_owner_only(&paths.core_database)?;
                import.import(&mut service)?;
                import.cutover(&mut service)?;
                let checksum = import.checksum().to_owned();
                (service, ActivationKind::ImportedNodeCache, Some(checksum))
            }
            (cache, true) => {
                let mut service = OperationService::open_existing(&paths.core_database)?;
                // A crash between the migration target's staged publish and
                // the import transaction leaves a PRISTINE (host-less)
                // authority: the re-import arm in `activate_existing` below
                // recovers exactly that state, so a missing host row is
                // tolerated — but only when a cache is available to import
                // from. Without one, a host-less authority is unrelated or
                // corrupt and keeps failing closed exactly as before.
                let host = if cache.is_some() {
                    service.host_optional()?
                } else {
                    Some(service.host()?)
                }
                .map(|record| record.identity);
                // Plan the cache only as far as it can be USED. For a
                // healthy, host-bearing authority the NodeCache beside the
                // Core database is the live compatibility projection
                // (M2.2b) — persisted downstream of Core execution and
                // mutated at any time — so its content must not gate the
                // activation: a cache that cannot be planned (e.g. the
                // runtime's own deferred-report queue,
                // `pending_control_plane`, populating exactly the field the
                // pristine-import invariant rejects — kubedoio/chv#343)
                // degrades to a warning and the projection is ignored,
                // exactly like the diagnostics in
                // `warn_about_unadopted_cache_content`. A host-less
                // authority genuinely needs the import (the crash-recovery
                // re-import arms in `activate_existing`), so there a plan
                // failure stays fatal.
                let import = match (host.is_some(), cache.as_deref()) {
                    (_, None) => None,
                    (true, Some(bytes)) => match plan(bytes) {
                        Ok(import) => Some(import),
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "retained NodeCache cannot be planned as an import source; \
                                 treating it as a live compatibility projection whose content \
                                 does not gate activation (the Core database is the authority)"
                            );
                            None
                        }
                    },
                    (false, Some(bytes)) => Some(plan(bytes)?),
                };
                resolve_host_identity(HostIdentityInputs {
                    existing_core: host,
                    importable_nodecache: import.as_ref().map(|value| value.host().clone()),
                    configured_seed,
                    precreation_enrollment,
                })?;
                let (kind, checksum) =
                    activate_existing(&paths, cache.as_deref(), import.as_ref(), &mut service)?;
                (service, kind, checksum)
            }
        };

        // The short cache transaction ends once the validated snapshot has
        // been consumed. Runtime database exclusion remains process-lifetime.
        let any_migration_state = service.has_any_migration_state()?;
        let foreign_migration_state = service.has_migration_state_other_than(SOURCE_NAME)?;
        let activated = ActivatedStore {
            service,
            kind,
            runtime_guard: RuntimeAuthorityGuard {
                _lease: runtime_lease,
            },
            provenance: ActivationProvenance {
                source_checksum,
                live_cache_present,
                any_migration_state,
                foreign_migration_state,
            },
            native_only: false,
        };
        Ok(PendingActivatedStore {
            activated,
            node_cache_path: paths.node_cache,
            cache: retained_cache,
            authority_lock,
        })
    }
}

#[derive(Debug, Error)]
pub enum StartupError {
    #[error("core-native mode refuses a legacy NodeCache source")]
    LegacyCachePresent,
    #[error("Core has an imported snapshot but the source NodeCache is missing")]
    ImportedSourceMissing,
    #[error("NodeCache checksum disagrees with the persisted Core migration marker")]
    ChecksumMismatch,
    #[error("Core database and NodeCache coexist without a NodeCache migration marker")]
    UnrelatedAuthority,
    #[error(
        "migration archive exists beside a markerless Core database, but NodeCache is missing"
    )]
    InterruptedMigrationSourceMissing,
    #[error(
        "NodeCache archive at {} is missing or does not match the persisted migration marker; \
         restore the archive (or the original cache it was made from) to boot this authority",
        path.display()
    )]
    ArchiveMismatch { path: PathBuf },
    #[error(
        "torn temporary archive at {} does not match the archive being written; \
         remove it (a torn write from a crashed boot) to boot this authority",
        temp.display()
    )]
    TornArchiveTemp { temp: PathBuf },
    #[error("archive path has no parent directory")]
    InvalidArchivePath,
    #[error("unsafe authority path configuration: {0}")]
    UnsafePath(String),
    #[error("I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Migration(#[from] MigrationError),
    #[error(transparent)]
    Operations(#[from] OperationServiceError),
    #[error(transparent)]
    Identity(#[from] HostIdentityError),
}

pub type Result<T> = std::result::Result<T, StartupError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    ArchiveSynced,
    ArchiveRenamed,
}

fn activate_existing(
    paths: &StartupPaths,
    cache: Option<&[u8]>,
    import: Option<&cellhv_nodecache_migration::ImportPlan>,
    service: &mut OperationService,
) -> Result<(ActivationKind, Option<String>)> {
    let marker = service.legacy_migration_state(SOURCE_NAME)?;
    match (cache, import, marker) {
        (None, None, None) => {
            if paths
                .node_cache_archive
                .try_exists()
                .map_err(|source| io_error(&paths.node_cache_archive, source))?
            {
                return Err(StartupError::InterruptedMigrationSourceMissing);
            }
            Ok((ActivationKind::Existing, None))
        }
        (None, None, Some(marker)) if marker.cutover => {
            verify_archive(&paths.node_cache_archive, &marker.checksum)?;
            Ok((ActivationKind::ImportedNodeCache, Some(marker.checksum)))
        }
        (None, None, Some(_)) => Err(StartupError::ImportedSourceMissing),
        (Some(bytes), Some(import), None) => {
            if service.host_optional()?.is_some() {
                // A live authority with no migration marker was never
                // migrated from this cache: the NodeCache beside it is the
                // core-managed compatibility projection (M2.2b), persisted
                // downstream of Core execution and mutated at any time.
                // The Core database is the authority — open it and leave
                // the live cache to the projection instead of demanding a
                // pristine import target (which a used authority can never
                // be).
                warn_about_unadopted_cache_content(import, service);
                Ok((ActivationKind::Existing, None))
            } else if !service.is_pristine_migration_target()? {
                Err(StartupError::UnrelatedAuthority)
            } else {
                archive_exact(&paths.node_cache_archive, bytes, &mut |_| Ok(()))?;
                import.import(service)?;
                import.cutover(service)?;
                Ok((
                    ActivationKind::ImportedNodeCache,
                    Some(import.checksum().to_owned()),
                ))
            }
        }
        (Some(bytes), Some(import), Some(marker)) => {
            if marker.cutover {
                // The migration already cut over on an earlier boot: the
                // live NodeCache is the compatibility projection and may
                // legitimately differ from the archived migration source.
                // Verify the retained archive (the exact source bytes) and
                // open the existing authority without re-importing. The
                // archive self-heals from the live cache when the cache
                // still hashes to the marker checksum (a backup or cleanup
                // that dropped the auxiliary archive file must not brick
                // the node); a mutated cache cannot stand in for the
                // source and fails closed.
                if let Err(error) = verify_archive(&paths.node_cache_archive, &marker.checksum) {
                    if import.checksum() == marker.checksum {
                        restore_archive(&paths.node_cache_archive, bytes, &marker.checksum)?;
                    } else {
                        return Err(error);
                    }
                }
                warn_about_unadopted_cache_content(import, service);
                Ok((ActivationKind::ImportedNodeCache, Some(marker.checksum)))
            } else {
                if import.checksum() != marker.checksum {
                    return Err(StartupError::ChecksumMismatch);
                }
                archive_exact(&paths.node_cache_archive, bytes, &mut |_| Ok(()))?;
                let disposition = import.import(service)?;
                debug_assert_eq!(disposition, MigrationDisposition::Replay);
                import.cutover(service)?;
                Ok((ActivationKind::ImportedNodeCache, Some(marker.checksum)))
            }
        }
        // An unplannable cache (import=None) reaches these arms only for
        // a healthy, host-bearing authority — `prepare_activation`
        // degrades plan failures to None exactly and only there. The
        // projection's content must not gate activation; the Core
        // database is the authority (kubedoio/chv#343).
        (Some(_), None, None) => {
            if service.host_optional()?.is_some() {
                Ok((ActivationKind::Existing, None))
            } else {
                // Unreachable from `prepare_activation` (a host-less
                // authority always carries a planned import or has already
                // failed closed) — kept as a defensive fail-closed.
                Err(StartupError::UnsafePath(
                    "inconsistent NodeCache activation snapshot".to_owned(),
                ))
            }
        }
        (Some(_), None, Some(marker)) if marker.cutover => {
            // Migrated authority whose live projection cannot be planned:
            // same contract as the cutover arm above minus the
            // self-heal/diagnostics (both need a plannable cache) — the
            // archived migration source alone proves the authority.
            verify_archive(&paths.node_cache_archive, &marker.checksum)?;
            Ok((ActivationKind::ImportedNodeCache, Some(marker.checksum)))
        }
        _ => Err(StartupError::UnsafePath(
            "inconsistent NodeCache activation snapshot".to_owned(),
        )),
    }
}

/// Warns when a live cache that this activation did NOT import references
/// VMs the Core authority does not know. In the designed steady state the
/// compatibility projection mirrors the authority's VMs, so this only
/// happens for a stale projection or a restored legacy cache that was
/// silently left unadopted (the Core database wins by design) — an
/// operator-attention event, and the agent-side startup rebuild refuses to
/// overwrite such a cache. Diagnostics must never fail the activation: on
/// any listing error the warning is skipped.
fn warn_about_unadopted_cache_content(
    import: &cellhv_nodecache_migration::ImportPlan,
    service: &OperationService,
) {
    let Ok(authority_vms) = service.vms() else {
        return;
    };
    let authority_ids: std::collections::HashSet<&str> = authority_vms
        .iter()
        .map(|definition| definition.id.as_str())
        .collect();
    let unadopted: Vec<&str> = import
        .definitions()
        .iter()
        .filter(|definition| !authority_ids.contains(definition.id.as_str()))
        .map(|definition| definition.id.as_str())
        .collect();
    if !unadopted.is_empty() {
        tracing::warn!(
            unadopted = ?unadopted,
            "live NodeCache references VMs the Core authority does not know — \
             a stale projection or a restored legacy cache was NOT imported; \
             the Core database is authoritative and the on-disk cache will \
             not be overwritten by the startup rebuild"
        );
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(StartupError::UnsafePath(format!(
                "{} is not a regular file",
                path.display()
            )))
        }
        Ok(metadata) => validate_owner_file(path, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(path, source)),
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error(path, source)),
    }
}

fn validate_paths(paths: &StartupPaths) -> Result<()> {
    let parents = [
        safe_parent(&paths.node_cache)?,
        safe_parent(&paths.core_database)?,
        safe_parent(&paths.node_cache_archive)?,
    ];
    for parent in parents {
        let metadata = fs::symlink_metadata(&parent).map_err(|source| io_error(&parent, source))?;
        if !metadata.file_type().is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || (metadata.permissions().mode() & 0o777 != 0o700
                && metadata.permissions().mode() & 0o777 != 0o750)
        {
            return Err(StartupError::UnsafePath(format!(
                "{} must be an owner-owned 0700 or 0750 directory",
                parent.display()
            )));
        }
    }
    let configured_paths = [
        &paths.node_cache,
        &paths.core_database,
        &paths.node_cache_archive,
    ];
    for path in configured_paths {
        match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(StartupError::UnsafePath(format!(
                    "{} is not a regular file",
                    path.display()
                )))
            }
            Ok(metadata) => validate_owner_file(path, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(path, source)),
        }
    }
    let archive_temp = archive_temp_path(&paths.node_cache_archive);
    let authority_lock = cellhv_core_fs::lock_path(&paths.node_cache)
        .map_err(|error| StartupError::UnsafePath(error.to_string()))?;
    let runtime_lease = cellhv_core_fs::runtime_lease_path(&paths.core_database)
        .map_err(|error| StartupError::UnsafePath(error.to_string()))?;
    let database_wal = PathBuf::from(format!("{}-wal", paths.core_database.display()));
    let database_shm = PathBuf::from(format!("{}-shm", paths.core_database.display()));
    let all_paths = [
        paths.node_cache.clone(),
        paths.core_database.clone(),
        paths.node_cache_archive.clone(),
        archive_temp,
        authority_lock,
        runtime_lease,
        database_wal,
        database_shm,
    ];
    for (index, left) in all_paths.iter().enumerate() {
        for right in &all_paths[index + 1..] {
            if normalize(left)? == normalize(right)? {
                return Err(StartupError::UnsafePath("authority paths alias".to_owned()));
            }
            if let (Ok(a), Ok(b)) = (fs::metadata(left), fs::metadata(right)) {
                if a.dev() == b.dev() && a.ino() == b.ino() {
                    return Err(StartupError::UnsafePath(
                        "authority paths are hardlink aliases".to_owned(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn safe_parent(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| StartupError::UnsafePath(format!("{} has no parent", path.display())))?;
    let metadata = fs::symlink_metadata(parent).map_err(|source| io_error(parent, source))?;
    if !metadata.file_type().is_dir() {
        return Err(StartupError::UnsafePath(format!(
            "{} must be a real directory",
            parent.display()
        )));
    }
    parent
        .canonicalize()
        .map_err(|source| io_error(parent, source))
}

fn normalize(path: &Path) -> Result<PathBuf> {
    Ok(safe_parent(path)?.join(
        path.file_name()
            .ok_or_else(|| StartupError::UnsafePath("path has no filename".to_owned()))?,
    ))
}

fn validate_owner_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(StartupError::UnsafePath(format!(
            "{} must be owner-owned, owner-only, and have one link",
            path.display()
        )));
    }
    Ok(())
}

fn set_owner_only(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))
}

fn verify_archive(path: &Path, checksum: &str) -> Result<()> {
    let bytes = read_optional(path)?.ok_or_else(|| StartupError::ArchiveMismatch {
        path: path.to_owned(),
    })?;
    if hex::encode(Sha256::digest(bytes)) != checksum {
        return Err(StartupError::ArchiveMismatch {
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// Re-creates the migration archive from bytes that have been verified to
/// hash to the marker's source checksum (the caller holds that proof): the
/// bytes ARE the exact migration source, so rewriting the archive from them
/// restores the retained-source invariant without weakening the tamper
/// evidence. Crash-safe by idempotence: a crash mid-restore leaves the
/// archive absent and the next boot repeats the same verified restore. The
/// temp is removed first so a crash *during* the restore's temp write also
/// heals on the next boot — a leftover temp cannot match the verified
/// source and would otherwise wedge every later boot.
fn restore_archive(path: &Path, bytes: &[u8], checksum: &str) -> Result<()> {
    debug_assert_eq!(hex::encode(Sha256::digest(bytes)), checksum);
    let temp = archive_temp_path(path);
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&temp, source)),
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(path, source)),
    }
    archive_exact(path, bytes, &mut |_| Ok(()))
}

fn archive_exact(
    path: &Path,
    bytes: &[u8],
    hook: &mut impl FnMut(Step) -> io::Result<()>,
) -> Result<()> {
    if let Some(existing) = read_optional(path)? {
        if Sha256::digest(existing) != Sha256::digest(bytes) {
            return Err(StartupError::ArchiveMismatch {
                path: path.to_owned(),
            });
        }
        let parent = path.parent().ok_or(StartupError::InvalidArchivePath)?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|source| io_error(parent, source))?;
        return Ok(());
    }
    let parent = path.parent().ok_or(StartupError::InvalidArchivePath)?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    let temp = archive_temp_path(path);
    if let Some(existing) = read_optional(&temp)? {
        if Sha256::digest(existing) != Sha256::digest(bytes) {
            return Err(StartupError::TornArchiveTemp {
                temp: temp.to_owned(),
            });
        }
        File::open(&temp)
            .and_then(|file| file.sync_all())
            .map_err(|source| io_error(&temp, source))?;
        fs::rename(&temp, path).map_err(|source| io_error(path, source))?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|source| io_error(parent, source))?;
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|source| io_error(&temp, source))?;
    if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(io_error(&temp, source));
    }
    hook(Step::ArchiveSynced).map_err(|source| io_error(&temp, source))?;
    fs::rename(&temp, path).map_err(|source| io_error(path, source))?;
    set_owner_only(path)?;
    hook(Step::ArchiveRenamed).map_err(|source| io_error(path, source))?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|source| io_error(parent, source))?;
    Ok(())
}

fn archive_temp_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|v| v.to_str())
            .unwrap_or("archive")
    ))
}

fn io_error(path: &Path, source: io::Error) -> StartupError {
    StartupError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cache_bytes(node_id: &str, observed_generation: &str) -> Vec<u8> {
        let spec = serde_json::to_vec(&json!({
            "name":"legacy-vm", "cpus":2, "memory_bytes":1073741824_u64,
            "kernel_path":"/kernel", "disks":[], "nics":[], "desired_state":"Running"
        }))
        .unwrap();
        serde_json::to_vec(&json!({
            "cache_version":1, "node_id":node_id,
            "observed_generation":observed_generation,
            "node_state":"TenantReady", "enrollment_complete":true,
            "vm_generations":{"vm-a":"3"}, "volume_generations":{}, "network_generations":{},
            "vm_fragments":{"vm-a":{"id":"vm-a","kind":"vm","generation":"3",
                "spec_json":spec,"policy_json":b"{}","updated_at":"2026-07-21T00:00:00Z","updated_by":"controller"}},
            "volume_fragments":{}, "network_fragments":{}, "vm_attachments":{},
            "volume_handles":{}, "pending_control_plane":[]
        })).unwrap()
    }

    fn source() -> Vec<u8> {
        cache_bytes("node-a", "7")
    }

    fn test_paths(dir: &tempfile::TempDir) -> StartupPaths {
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        StartupPaths {
            node_cache: dir.path().join("node-cache.json"),
            core_database: dir.path().join("core.db"),
            node_cache_archive: dir.path().join("node-cache-v1.archive"),
        }
    }

    fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn startup_transaction_yields_open_fresh_authority_and_restart_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let transaction = StartupTransaction::begin(&paths).unwrap();
        let active = transaction
            .activate(Some("host-transaction".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::Fresh);
        assert_eq!(
            active.service().host().unwrap().identity.id.as_str(),
            "host-transaction"
        );
        let (service, kind, runtime_guard, provenance) = active.into_runtime_parts();
        assert_eq!(kind, ActivationKind::Fresh);
        assert!(provenance.source_checksum().is_none());
        assert!(!provenance.live_cache_present());
        assert!(matches!(
            StartupTransaction::begin(&paths),
            Err(StartupError::Io { source, .. })
                if source.kind() == io::ErrorKind::WouldBlock
        ));

        drop(service);
        drop(runtime_guard);
        let restarted = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("host-transaction".to_owned()), None)
            .unwrap();
        assert_eq!(restarted.kind(), ActivationKind::Existing);
        assert_eq!(
            restarted.service().host().unwrap().identity.id.as_str(),
            "host-transaction"
        );
    }

    #[test]
    fn activated_store_releases_cache_lock_but_retains_runtime_lease() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let transaction = StartupTransaction::begin(&paths).unwrap();
        let active = transaction
            .activate(Some("host-lock-held".to_owned()), None)
            .unwrap();
        let cache_guard = cellhv_core_fs::AuthorityLock::acquire(&paths.node_cache).unwrap();
        assert!(matches!(
            StartupTransaction::begin(&paths),
            Err(StartupError::Io { source, .. })
                if source.kind() == io::ErrorKind::WouldBlock
        ));
        drop(cache_guard);
        drop(active);
        drop(StartupTransaction::begin(&paths).unwrap());
    }

    #[test]
    fn startup_transaction_rejects_runtime_lease_aliases_before_locking() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = test_paths(&dir);
        paths.node_cache = cellhv_core_fs::runtime_lease_path(&paths.core_database).unwrap();
        assert!(matches!(
            StartupTransaction::begin(&paths),
            Err(StartupError::UnsafePath(_))
        ));
        assert!(!paths.core_database.exists());
    }

    #[test]
    fn startup_transaction_imports_exact_snapshot_and_restarts_under_one_lease() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, source());
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::ImportedNodeCache);
        assert!(paths.node_cache_archive.exists());
        assert_eq!(
            active.service().host().unwrap().identity.id.as_str(),
            "node-a"
        );
        drop(active);
        fs::remove_file(&paths.node_cache).unwrap();

        let restarted = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(restarted.kind(), ActivationKind::ImportedNodeCache);
        assert_eq!(
            restarted.service().host().unwrap().identity.id.as_str(),
            "node-a"
        );
    }

    #[test]
    fn interrupted_migration_target_with_cache_recovers_by_reimport() {
        // Crash window: the migration target was published (staged rename)
        // but the import transaction never ran — the authority is pristine
        // and HOST-LESS while the node cache still exists. The next boot
        // must take the (cache, database-exists) arm, tolerate the missing
        // host row, and recover through the pristine re-import path instead
        // of failing on `host()` before the recovery arm is reachable
        // (the R2 review caught that this window bricked startup).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, source());

        // Reproduce the post-crash on-disk state exactly: a published,
        // schema-complete, host-less migration target at the final path.
        {
            let service = OperationService::create_migration_target(&paths.core_database)
                .expect("migration target bootstrap");
            assert!(
                service.is_pristine_migration_target().unwrap(),
                "a fresh migration target must be pristine (host-less)"
            );
        }

        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::ImportedNodeCache);
        assert_eq!(
            active.service().host().unwrap().identity.id.as_str(),
            "node-a"
        );
    }

    /// Like [`cache_bytes`], but with the runtime's deferred-report queue
    /// non-empty — the exact live-projection state a control-plane outage
    /// produces (the agent persists `pending_control_plane` on every defer;
    /// the M4.3 run-4 reproduction had 28 queued) and the pristine-import
    /// invariant rejects (kubedoio/chv#343).
    fn cache_bytes_with_deferred_reports(node_id: &str, observed_generation: &str) -> Vec<u8> {
        let mut value: serde_json::Value =
            serde_json::from_slice(&cache_bytes(node_id, observed_generation)).unwrap();
        value["pending_control_plane"] = json!([{"kind": "NodeStateReport"}]);
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn healthy_authority_boots_with_unplannable_projection() {
        // kubedoio/chv#343 regression: a healthy, host-bearing authority
        // must boot when the NodeCache beside it (the live compatibility
        // projection) holds deferred control-plane reports. Previously
        // every boot ran the full import validation on the projection and
        // failed closed with Migration(Unsupported("pending_control_plane")),
        // bricking the agent after "outage + restart" — the exact recovery
        // path the deferral feature exists for.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        // First boot: fresh authority, no cache (the deployment shape).
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("host-transaction".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::Fresh);
        drop(active);

        // The projection accumulates deferred reports during an outage.
        write_private(
            &paths.node_cache,
            cache_bytes_with_deferred_reports("host-transaction", "9"),
        );

        // Restart must succeed: the projection's content does not gate
        // activation; the Core database is the authority.
        let restarted = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("host-transaction".to_owned()), None)
            .unwrap();
        assert_eq!(restarted.kind(), ActivationKind::Existing);
        assert_eq!(
            restarted.service().host().unwrap().identity.id.as_str(),
            "host-transaction"
        );
    }

    #[test]
    fn cutover_authority_boots_with_unplannable_projection() {
        // Same poison, migrated-authority shape: the cutover marker and
        // archived source exist, and the live projection has since
        // mutated (deferred reports). The archive alone must prove the
        // authority — the mutated, unplannable projection must neither
        // brick the boot nor stand in for the source.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, source());
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::ImportedNodeCache);
        assert!(paths.node_cache_archive.exists());
        drop(active);

        write_private(
            &paths.node_cache,
            cache_bytes_with_deferred_reports("node-a", "9"),
        );
        let restarted = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(restarted.kind(), ActivationKind::ImportedNodeCache);
        assert_eq!(
            restarted.service().host().unwrap().identity.id.as_str(),
            "node-a"
        );
    }

    #[test]
    fn cutover_authority_with_unplannable_cache_still_requires_archive() {
        // Fail-closed preserved: a migrated authority whose projection
        // cannot be planned (so it cannot self-heal the archive) still
        // refuses to boot without the archived migration source — the
        // relaxed projection handling must not weaken authority proof.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, source());
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        drop(active);

        fs::remove_file(&paths.node_cache_archive).unwrap();
        write_private(
            &paths.node_cache,
            cache_bytes_with_deferred_reports("node-a", "9"),
        );
        let error = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .err()
            .expect("activation must fail");
        assert!(matches!(
            error,
            StartupError::ArchiveMismatch { ref path }
                if path == &paths.node_cache_archive
        ));
    }

    #[test]
    fn hostless_authority_with_unplannable_cache_still_fails_closed() {
        // The crash-recovery window (pristine, host-less migration target)
        // genuinely needs to import the cache, so an unplannable cache
        // stays fatal there — the fix only relaxes healthy authorities.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        {
            let service = OperationService::create_migration_target(&paths.core_database)
                .expect("migration target bootstrap");
            assert!(service.is_pristine_migration_target().unwrap());
        }
        write_private(
            &paths.node_cache,
            cache_bytes_with_deferred_reports("node-a", "7"),
        );
        let error = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .err()
            .expect("activation must fail");
        assert!(matches!(
            error,
            StartupError::Migration(MigrationError::Unsupported(ref path))
                if path == "pending_control_plane"
        ));
    }

    #[test]
    fn initial_import_of_unplannable_cache_still_fails_closed() {
        // The (cache, no-database) import arm is unchanged: a cache with
        // deferred reports cannot be deterministically migrated and must
        // keep failing closed at import time.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(
            &paths.node_cache,
            cache_bytes_with_deferred_reports("node-a", "7"),
        );
        let error = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .err()
            .expect("activation must fail");
        assert!(matches!(
            error,
            StartupError::Migration(MigrationError::Unsupported(ref path))
                if path == "pending_control_plane"
        ));
    }

    #[test]
    fn hostless_authority_without_cache_still_fails_closed() {
        // The re-import tolerance is gated on a cache being available: a
        // host-less authority with nothing to import from is unrelated or
        // corrupt and must keep failing closed (unchanged behavior).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        drop(
            OperationService::create_migration_target(&paths.core_database)
                .expect("migration target bootstrap"),
        );

        let result = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None);
        assert!(
            matches!(result, Err(StartupError::Operations(_))),
            "host-less authority without a cache must fail closed"
        );
    }

    #[test]
    fn stale_staging_sibling_does_not_block_migration_target_bootstrap() {
        // Crash leftover from the staged publish: junk staging siblings in
        // the core directory — including names carrying THIS process's pid
        // (pid reuse after a crash), so the existence-checked allocation
        // must skip occupied candidates, not merely differ by pid. The
        // next bootstrap must pick a fresh staging name and succeed.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, source());
        let parent = paths.core_database.parent().unwrap();
        let name = paths
            .core_database
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        for sequence in 0..6 {
            let stale = parent.join(format!(".{}.fresh-{}-{sequence}", name, std::process::id()));
            fs::write(&stale, b"interrupted staging garbage").unwrap();
        }
        // And one from a different (restarted) pid.
        fs::write(
            parent.join(format!(".{}.fresh-999999-0", name)),
            b"interrupted staging garbage",
        )
        .unwrap();

        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::ImportedNodeCache);
        assert_eq!(
            active.service().host().unwrap().identity.id.as_str(),
            "node-a"
        );
    }

    #[test]
    fn live_projection_cache_beside_established_authority_is_ignored() {
        // Steady state of a core-managed node born fresh (never migrated):
        // the compatibility projection persists agent-cache.json downstream
        // of Core execution, so every restart after the first sees
        // (cache, database). The live authority carries no migration
        // marker — the cache is a projection artifact, not a migration
        // source — so activation must open the existing authority instead
        // of demanding a pristine import target (`UnrelatedAuthority` on
        // every restart was the M2.5 real-KVM finding).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("host-projection".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::Fresh);
        drop(active);

        // The projection persists a cache beside the authority (same node
        // identity, mutated generation).
        write_private(&paths.node_cache, cache_bytes("host-projection", "9"));
        let restarted = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("host-projection".to_owned()), None)
            .unwrap();
        assert_eq!(restarted.kind(), ActivationKind::Existing);
        assert_eq!(
            restarted.service().host().unwrap().identity.id.as_str(),
            "host-projection"
        );
        assert!(restarted.provenance().source_checksum().is_none());
        assert!(restarted.provenance().live_cache_present());
        assert!(!restarted.provenance().has_any_migration_state());
        assert!(!restarted.provenance().has_foreign_migration_state());
        // No import ran: no archive must exist, and the projection
        // artifact must survive untouched for the compatibility surface.
        assert!(!paths.node_cache_archive.exists());
        assert!(paths.node_cache.exists());
    }

    #[test]
    fn cutover_marker_with_mutated_projection_cache_opens_existing() {
        // Legacy cutover boot imports the cache and marks the cutover
        // complete. From the next boot on, the compatibility projection
        // owns the live cache and its bytes legitimately diverge from the
        // archived migration source. The steady state must verify the
        // retained archive and open the authority (previously:
        // `ChecksumMismatch` on every boot after the first projection
        // save).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, cache_bytes("node-a", "7"));
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::ImportedNodeCache);
        assert!(paths.node_cache_archive.exists());
        drop(active);

        // The projection saves a mutated cache after the cutover (same
        // node identity, new generation) — its checksum now differs from
        // the migration marker.
        write_private(&paths.node_cache, cache_bytes("node-a", "9"));
        let restarted = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(restarted.kind(), ActivationKind::ImportedNodeCache);
        assert_eq!(
            restarted.service().host().unwrap().identity.id.as_str(),
            "node-a"
        );
        assert!(restarted.provenance().source_checksum().is_some());
        assert!(restarted.provenance().has_any_migration_state());
        // The archive still holds the exact migration source.
        assert!(paths.node_cache_archive.exists());
    }

    #[test]
    fn fresh_native_only_activation_carries_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let native = StartupTransaction::begin(&paths)
            .unwrap()
            .activate_native_only(Some("host-native".to_owned()))
            .unwrap();
        assert!(native.native_only());
        assert_eq!(native.kind(), ActivationKind::Fresh);
        assert!(native.provenance().source_checksum().is_none());
    }

    #[test]
    fn native_only_activation_over_migrated_authority_keeps_provenance_visible() {
        // A database migrated by the managed composition (marker present)
        // must keep its migration provenance visible so the native-only
        // runtime fence stays fail-closed, while the same state remains
        // bootable by the managed composition.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, cache_bytes("node-a", "7"));
        let managed = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert!(!managed.native_only());
        assert_eq!(managed.kind(), ActivationKind::ImportedNodeCache);
        drop(managed);

        fs::remove_file(&paths.node_cache).unwrap();
        let native = StartupTransaction::begin(&paths)
            .unwrap()
            .activate_native_only(Some("node-a".to_owned()))
            .unwrap();
        assert!(native.native_only());
        assert_eq!(native.kind(), ActivationKind::ImportedNodeCache);
        assert!(native.provenance().source_checksum().is_some());
        assert!(native.provenance().has_any_migration_state());
        // The cutover marker is the agent's own source, not a foreign one.
        assert!(!native.provenance().has_foreign_migration_state());
    }

    #[test]
    fn archive_deleted_after_cutover_self_heals_from_matching_cache() {
        // Review finding 2: a backup/cleanup that drops the auxiliary
        // archive file must not brick the node when the live cache still
        // hashes to the marker checksum — it IS the exact migration source
        // and the archive is rewritten from it.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, cache_bytes("node-a", "7"));
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(active.kind(), ActivationKind::ImportedNodeCache);
        drop(active);
        assert!(paths.node_cache_archive.exists());

        fs::remove_file(&paths.node_cache_archive).unwrap();
        let healed = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(healed.kind(), ActivationKind::ImportedNodeCache);
        // The archive was restored from the verified live cache.
        assert!(verify_archive(
            &paths.node_cache_archive,
            &hex::encode(Sha256::digest(cache_bytes("node-a", "7")))
        )
        .is_ok());
    }

    #[test]
    fn corrupted_archive_after_cutover_self_heals_from_matching_cache() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, cache_bytes("node-a", "7"));
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        drop(active);

        write_private(&paths.node_cache_archive, b"corrupted-bytes");
        let healed = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(healed.kind(), ActivationKind::ImportedNodeCache);
        assert!(verify_archive(
            &paths.node_cache_archive,
            &hex::encode(Sha256::digest(cache_bytes("node-a", "7")))
        )
        .is_ok());
    }

    #[test]
    fn torn_temp_from_crashed_restore_self_heals_on_next_boot() {
        // Review follow-up on the self-heal: a crash *during* the restore's
        // temp write leaves a torn temp beside the removed archive. The
        // verified restore must remove the temp instead of wedging every
        // later boot on its digest mismatch.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, cache_bytes("node-a", "7"));
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        drop(active);

        fs::remove_file(&paths.node_cache_archive).unwrap();
        write_private(
            &archive_temp_path(&paths.node_cache_archive),
            b"torn-write-bytes",
        );
        let healed = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        assert_eq!(healed.kind(), ActivationKind::ImportedNodeCache);
        assert!(verify_archive(
            &paths.node_cache_archive,
            &hex::encode(Sha256::digest(cache_bytes("node-a", "7")))
        )
        .is_ok());
        assert!(!archive_temp_path(&paths.node_cache_archive).exists());
    }

    #[test]
    fn torn_temp_beside_absent_archive_fails_closed_naming_the_temp() {
        // Outside the verified-restore path (an initial import whose temp was
        // torn by a crash and whose source has since changed) the mismatch
        // stays a hard failure, but the error names the temp file so the
        // recovery (remove it) is actionable.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(
            &archive_temp_path(&paths.node_cache_archive),
            b"torn-write-bytes",
        );
        write_private(&paths.node_cache, cache_bytes("node-a", "7"));
        let error = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .err()
            .expect("activation must fail");
        assert!(matches!(
            error,
            StartupError::TornArchiveTemp { ref temp }
                if temp == &archive_temp_path(&paths.node_cache_archive)
        ));
        assert!(error.to_string().contains(".tmp"));
    }

    #[test]
    fn archive_missing_with_mutated_cache_fails_closed_actionably() {
        // A mutated cache cannot stand in for the migration source: the
        // failure must stay closed and name the archive path so an operator
        // can act on it (restore the archive or the original cache).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, cache_bytes("node-a", "7"));
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        drop(active);

        fs::remove_file(&paths.node_cache_archive).unwrap();
        write_private(&paths.node_cache, cache_bytes("node-a", "9"));
        let error = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .err()
            .expect("activation must fail");
        assert!(matches!(
            error,
            StartupError::ArchiveMismatch { ref path }
                if path == &paths.node_cache_archive
        ));
        assert!(error.to_string().contains("node-cache-v1.archive"));
    }

    #[test]
    fn crash_after_import_before_cutover_with_mutated_cache_fails_closed() {
        // Probe X2: an import interrupted before cutover, with the live
        // cache mutated after the fact, must not be re-imported — the
        // checksum the marker recorded is the only admissible source.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let original = cache_bytes("node-a", "7");
        let checksum = hex::encode(Sha256::digest(&original));
        let mut service = OperationService::create_migration_target(&paths.core_database).unwrap();
        let host = cellhv_core_types::HostIdentity {
            id: cellhv_core_types::HostId::new("node-a").unwrap(),
            resource_version: cellhv_core_types::ResourceVersion::new(1).unwrap(),
        };
        service
            .import_legacy_snapshot(SOURCE_NAME, &checksum, &host, &[])
            .unwrap();
        drop(service);

        write_private(&paths.node_cache, cache_bytes("node-a", "9"));
        let error = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .err()
            .expect("activation must fail");
        assert!(matches!(error, StartupError::ChecksumMismatch));
    }

    #[test]
    fn disagreeing_identity_cache_beside_established_authority_fails_closed() {
        // The ignore arm must never adopt or even tolerate a cache whose
        // node identity disagrees with the DB host: identity resolution
        // refuses before any activation arm runs.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        write_private(&paths.node_cache, cache_bytes("node-a", "7"));
        let active = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .unwrap();
        drop(active);

        // An operator restores a different node's cache beside the
        // authority.
        write_private(&paths.node_cache, cache_bytes("node-b", "7"));
        let error = StartupTransaction::begin(&paths)
            .unwrap()
            .activate(Some("node-a".to_owned()), None)
            .err()
            .expect("activation must fail");
        assert!(matches!(
            error,
            StartupError::Identity(HostIdentityError::Conflict { .. })
        ));
    }
}
