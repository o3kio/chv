//! Strongly-typed in-memory representation of a CHVArchitecture YAML
//! document, version `chv.kubedo.io/v1alpha1`.
//!
//! Field naming mirrors `docs/specs/architecture-designer/contracts/yaml-contract.md`
//! exactly. `apiVersion` is renamed via `serde(rename)`; all other fields use
//! their YAML names directly so the snake_case contract is preserved on the
//! wire without a global rename.
//!
//! # Why `cidr`/`gateway`/IPs are `String`, not `IpNetwork`/`IpAddr`
//!
//! Static checks need to emit a *finding* for an invalid CIDR (code
//! `INVALID_CIDR`), not fail to deserialise. If the model were strongly
//! typed at the parse layer, a single bad CIDR would produce a serde error
//! and the user would never see the rest of the diagnostics. We keep these
//! fields as `String` and validate them in `static_checks`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Top-level CHVArchitecture document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CHVArchitecture {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub servers: Vec<Server>,
    #[serde(default)]
    pub networks: Vec<Network>,
    #[serde(default)]
    pub datastores: Vec<Datastore>,
    #[serde(default)]
    pub backup_targets: Vec<BackupTarget>,
    #[serde(default)]
    pub backup_policies: Vec<BackupPolicy>,
    #[serde(default)]
    pub images: Vec<Image>,
    #[serde(default)]
    pub templates: Vec<Template>,
    #[serde(default)]
    pub instances: Vec<Instance>,
    #[serde(default)]
    pub ssh_keys: Vec<SshKey>,
    #[serde(default)]
    pub instance_users: Vec<InstanceUser>,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub projects: Vec<Project>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub management_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ServerRole>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ServerResources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub networks: Option<ServerNetworks>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerRole {
    Compute,
    Storage,
    Network,
    Management,
    Mixed,
    /// Forward-compat fallback; YAML enum may evolve.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_gb: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerNetworks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<ServerInterface>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerInterface {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Network {
    pub name: String,
    #[serde(rename = "type")]
    pub network_type: NetworkType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cidr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dhcp: Option<DhcpConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkType {
    Bridge,
    Vlan,
    Nat,
    Isolated,
    Routed,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DhcpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_end: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Datastore {
    pub name: String,
    #[serde(rename = "type")]
    pub datastore_type: DatastoreType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<DatastoreCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DatastoreCapabilities {
    #[serde(default)]
    pub snapshots: bool,
    #[serde(default)]
    pub thin_provisioning: bool,
    #[serde(default)]
    pub online_resize: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DatastoreType {
    #[serde(rename = "qcow2-dir")]
    Qcow2Dir,
    #[serde(rename = "ceph-rbd")]
    CephRbd,
    Nfs,
    Lvm,
    Zfs,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackupTarget {
    pub name: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datastore: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackupPolicy {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<BackupRetention>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackupRetention {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Image {
    pub name: String,
    pub source: String,
    pub format: ImageFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datastore: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Qcow2,
    Raw,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datastore: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<InstancePlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<InstanceResources>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<InstanceDisk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<InstanceNetwork>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_init: Option<InstanceCloudInit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<InstanceBackup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstancePlacement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceDisk {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datastore: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceNetwork {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceCloudInit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<InstanceCloudInitUser>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceCloudInitUser {
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceBackup {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SshKey {
    pub name: String,
    pub public_key: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceUser {
    pub name: String,
    #[serde(default)]
    pub sudo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_authorized_keys: Vec<SshKeyRef>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SshKeyRef {
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<UserAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserAuth {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// Project definitions are intentionally typed as a free-form object map.
/// `yaml-contract.md` does not yet pin a project schema, and the JSON
/// Schema accepts `additionalProperties: true` here. Keeping it as a
/// `serde_json::Value`-shaped map preserves round-trip fidelity until the
/// contract narrows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Project {
    pub name: String,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}
