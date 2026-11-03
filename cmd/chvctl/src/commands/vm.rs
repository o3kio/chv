use clap::Subcommand;
use serde_json::json;

use crate::client::{BffClient, CliError};
use crate::output::{self, OutputFormat};

#[derive(Subcommand)]
pub enum VmCommands {
    /// List all virtual machines
    List,
    /// Get details of a specific VM
    Get {
        /// VM identifier
        vm_id: String,
    },
    /// Create a new virtual machine
    Create {
        /// Name for the new VM
        name: String,
        /// Number of vCPUs
        #[arg(long)]
        cpu: Option<u32>,
        /// Memory size (e.g. "2G", "512M")
        #[arg(long)]
        memory: Option<String>,
        /// Base image to use
        #[arg(long)]
        image: Option<String>,
        /// Network to attach
        #[arg(long)]
        network: Option<String>,
        /// Placement node (defaults to the most recently enrolled node)
        #[arg(long)]
        node: Option<String>,
        /// Boot volume storage class (local, iscsi, ceph, lvm) — validated
        /// against the same shared vocabulary the BFF checks (#372 DP9)
        #[arg(long)]
        storage_class: Option<String>,
        /// Boot volume size in GiB (#372 DP9)
        #[arg(long)]
        disk_size_gb: Option<i64>,
        /// Cloud-init userdata, or `@file` to read it from a file (#372 DP9)
        #[arg(long)]
        cloud_init: Option<String>,
    },
    /// Start a virtual machine
    Start {
        /// VM identifier
        vm_id: String,
    },
    /// Stop a virtual machine
    Stop {
        /// VM identifier
        vm_id: String,
    },
    /// Reboot a virtual machine
    Reboot {
        /// VM identifier
        vm_id: String,
    },
    /// Delete a virtual machine
    Delete {
        /// VM identifier
        vm_id: String,
    },
    /// Migrate a VM to another node
    Migrate {
        /// VM identifier
        vm_id: String,
        /// Target node ID
        #[arg(long)]
        to: String,
        /// Required: pause the VM for the full transfer (stop-the-world).
        /// Live migration is disabled until #394 is proven; omitting this
        /// flag fails closed without sending a request.
        #[arg(long)]
        pause_first: bool,
    },
    /// Resize a VM's resources
    Resize {
        /// VM identifier
        vm_id: String,
        /// New vCPU count
        #[arg(long)]
        cpu: Option<u32>,
        /// New memory size (e.g. "4G")
        #[arg(long)]
        memory: Option<String>,
    },
    /// Import a VM from a qcow2 disk image
    Import {
        /// Name for the imported VM
        name: String,
        /// Path to the qcow2 disk image file
        #[arg(long)]
        file: String,
    },
}

pub async fn execute(
    client: &BffClient,
    command: VmCommands,
    format: &OutputFormat,
) -> Result<(), CliError> {
    match command {
        VmCommands::List => {
            let resp = client.post("/v1/vms", &json!({})).await?;
            let items = resp
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            output::print_list(
                &items,
                &["vm_id", "name", "power_state", "node_id", "cpu", "memory"],
                format,
            );
        }
        VmCommands::Get { vm_id } => {
            let resp = client
                .post("/v1/vms/get", &json!({ "vm_id": vm_id }))
                .await?;
            output::print_value(&resp, format);
        }
        VmCommands::Create {
            name,
            cpu,
            memory,
            image,
            network,
            node,
            storage_class,
            disk_size_gb,
            cloud_init,
        } => {
            // Field names follow the BFF create_vm contract: cpu_count,
            // memory_bytes, image_ref, network_id (the legacy "cpu"/"memory"/
            // "image"/"network" spellings are not read by the handler and
            // were silently ignored, always creating a default-spec VM).
            // The #372 DP9 flags (node_id, storage_class, volume_size_gb,
            // cloud_init_userdata) map to the same handler's fields.
            let mut body = json!({ "name": name });
            if let Some(c) = cpu {
                body["cpu_count"] = json!(c);
            }
            if let Some(m) = memory {
                body["memory_bytes"] = json!(parse_size_bytes(&m)?);
            }
            if let Some(i) = image {
                body["image_ref"] = json!(i);
            }
            if let Some(n) = network {
                body["network_id"] = json!(n);
            }
            if let Some(n) = node {
                body["node_id"] = json!(n);
            }
            if let Some(class) = storage_class {
                // Trim before validating AND before sending (review round
                // 1, parity with the BFF): the BFF trims the class before
                // its vocabulary check, so an untrimmed client-side
                // compare would over-reject (e.g. a shell tab-completion
                // trailing space) — same vocabulary, same rules both
                // sides.
                let class = class.trim();
                validate_storage_class(class)?;
                body["storage_class"] = json!(class);
            }
            if let Some(gb) = disk_size_gb {
                // Mirror the BFF's full 1..=64-TiB bound so the client
                // rejects with the same rule the server enforces
                // (review round 1); the server check stays
                // authoritative. The ceiling is the single shared
                // constant (`chv_hypervisor_api::resources::
                // MAX_VOLUME_BYTES` — the shared-vocabulary home
                // chvctl already imports for the class list, per the
                // #513 PR 3 consolidation; previously a local literal),
                // derived here into the GiB units this flag and the
                // BFF's `volume_size_gb` field use.
                const MAX_VOLUME_SIZE_GB: i64 =
                    chv_hypervisor_api::resources::MAX_VOLUME_BYTES / 1024 / 1024 / 1024;
                if gb <= 0 || gb > MAX_VOLUME_SIZE_GB {
                    return Err(CliError::Parse(format!(
                        "invalid --disk-size-gb {gb}: must be between 1 and {MAX_VOLUME_SIZE_GB} GiB (64 TiB)"
                    )));
                }
                body["volume_size_gb"] = json!(gb);
            }
            if let Some(userdata) = cloud_init {
                body["cloud_init_userdata"] = json!(resolve_cloud_init_userdata(&userdata)?);
            }
            let resp = client.post("/v1/vms/create", &body).await?;
            println!("VM created successfully.");
            output::print_value(&resp, format);
        }
        VmCommands::Start { vm_id } => {
            let body = json!({ "vm_id": vm_id, "action": "start", "force": false });
            client.post("/v1/vms/mutate", &body).await?;
            println!("VM {vm_id} starting.");
        }
        VmCommands::Stop { vm_id } => {
            let body = json!({ "vm_id": vm_id, "action": "stop", "force": false });
            client.post("/v1/vms/mutate", &body).await?;
            println!("VM {vm_id} stopping.");
        }
        VmCommands::Reboot { vm_id } => {
            // The BFF mutation contract accepts "restart" (mapped to the
            // reboot_vm RPC); "reboot" is rejected as an invalid action.
            let body = json!({ "vm_id": vm_id, "action": "restart", "force": false });
            client.post("/v1/vms/mutate", &body).await?;
            println!("VM {vm_id} rebooting.");
        }
        VmCommands::Delete { vm_id } => {
            let body = json!({ "vm_id": vm_id });
            client.post("/v1/vms/delete", &body).await?;
            println!("VM {vm_id} deleted.");
        }
        VmCommands::Migrate {
            vm_id,
            to,
            pause_first,
        } => {
            crate::commands::require_pause_first(pause_first)?;
            let body = json!({
                "vm_id": vm_id,
                "action": "migrate",
                "target_node_id": to,
                "pause_first": pause_first,
            });
            client.post("/v1/vms/mutate", &body).await?;
            println!("VM {vm_id} migrating to node {to}.");
        }
        VmCommands::Resize { vm_id, cpu, memory } => {
            // Same BFF contract as create: cpu_count / memory_bytes (the
            // handler rejects the legacy "cpu"/"memory" spellings with a
            // 400 before this fix).
            let mut body = json!({ "vm_id": vm_id });
            if let Some(c) = cpu {
                body["cpu_count"] = json!(c);
            }
            if let Some(m) = memory {
                body["memory_bytes"] = json!(parse_size_bytes(&m)?);
            }
            client.post("/v1/vms/resize", &body).await?;
            println!("VM {vm_id} resized.");
        }
        VmCommands::Import { name, file } => {
            // The BFF import contract (`POST /v1/vms/import`, refs
            // #386/#481) is multipart — the route's whole request
            // shape is a `name` text field and a `file` binary field
            // whose first four bytes must be the qcow2 magic (the
            // server rejects anything else with a 400). The CSRF gate
            // requires a non-empty `x-csrf-token` header on multipart
            // (#531); the client's multipart method sends it — see
            // `BffClient::post_multipart`'s doc comment.
            let file_name = std::path::Path::new(&file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("disk.qcow2")
                .to_string();
            let bytes = read_import_file(&file)?;
            let resp = client
                .post_multipart("/v1/vms/import", &name, &file_name, bytes)
                .await?;
            println!("VM import initiated.");
            output::print_value(&resp, format);
        }
    }
    Ok(())
}

/// Validate `--storage-class` against the single shared DP3 vocabulary
/// (`chv_hypervisor_api::resources::BACKEND_CLASSES` — the same list the
/// BFF's create handler validates against, #372 DP9/#379). Rejecting
/// client-side keeps a typo from becoming a server round trip, but the
/// BFF check remains authoritative.
pub(crate) fn validate_storage_class(class: &str) -> Result<(), CliError> {
    if chv_hypervisor_api::resources::is_known_backend_class(class) {
        Ok(())
    } else {
        Err(CliError::Parse(format!(
            "unknown storage_class {class:?}: must be one of {}",
            chv_hypervisor_api::resources::BACKEND_CLASSES.join(", ")
        )))
    }
}

/// Resolve `--cloud-init <userdata-or-@file>`: a leading `@` reads the
/// userdata from the file, anything else is the userdata itself (#372
/// DP9 — the same convention curl uses).
pub(crate) fn resolve_cloud_init_userdata(input: &str) -> Result<String, CliError> {
    match input.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| CliError::Io(format!("failed to read cloud-init file {path:?}: {e}"))),
        None => Ok(input.to_string()),
    }
}

/// Read the qcow2 disk image `vm import --file` uploads (#532). The
/// server validates the qcow2 magic (a non-qcow2 file is a loud 400
/// naming it), so the client only proves the file is readable — the
/// same client-thin/server-authoritative split `--storage-class` uses.
/// A regular-file + size guard runs first (review-folded): an
/// unbounded `std::fs::read` would buffer a near-cap file entirely
/// into heap, and a device or FIFO path would read until OOM.
pub(crate) fn read_import_file(path: &str) -> Result<Vec<u8>, CliError> {
    /// Mirrors the server's `MAX_FILE_SIZE` (the import handler's
    /// 100 GiB cap) so an oversized file fails loudly client-side
    /// instead of buffering the bytes first.
    const IMPORT_FILE_MAX_BYTES: u64 = 100 * 1024 * 1024 * 1024;

    let meta = std::fs::metadata(path)
        .map_err(|e| CliError::Io(format!("failed to read image file {path:?}: {e}")))?;
    if !meta.is_file() {
        return Err(CliError::Io(format!(
            "image file {path:?} is not a regular file — refusing to read it \
             (a device or FIFO would read without end)"
        )));
    }
    if meta.len() > IMPORT_FILE_MAX_BYTES {
        return Err(CliError::Io(format!(
            "image file {path:?} is {} bytes — over the 100 GiB upload limit \
             the server enforces",
            meta.len()
        )));
    }
    std::fs::read(path)
        .map_err(|e| CliError::Io(format!("failed to read image file {path:?}: {e}")))
}

/// Parse a human size ("512M", "2G", "1.5GiB", "4096") into bytes.
///
/// Suffixes are binary (K/M/G/T = KiB/MiB/GiB/TiB, base 1024), matching the
/// BFF's MiB-based `memory_mb` handling and display formatting. A bare
/// number is bytes.
pub(crate) fn parse_size_bytes(input: &str) -> Result<i64, CliError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(CliError::Parse("empty size string".into()));
    }
    let (num_part, suffix) =
        s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
    let num: f64 = num_part
        .trim()
        .parse()
        .map_err(|_| CliError::Parse(format!("invalid size: {input:?}")))?;
    if num < 0.0 {
        return Err(CliError::Parse(format!("negative size: {input:?}")));
    }
    let mult: i64 = match suffix
        .trim_end_matches("B")
        .trim_end_matches("i")
        .to_uppercase()
        .as_str()
    {
        "" => 1,
        "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024_i64 * 1024 * 1024 * 1024,
        _ => return Err(CliError::Parse(format!("unknown size suffix: {input:?}"))),
    };
    let bytes = num * mult as f64;
    if !bytes.is_finite() || bytes > i64::MAX as f64 {
        return Err(CliError::Parse(format!("size out of range: {input:?}")));
    }
    Ok(bytes as i64)
}

#[cfg(test)]
mod tests {
    use super::{
        parse_size_bytes, read_import_file, resolve_cloud_init_userdata, validate_storage_class,
    };

    #[test]
    fn parses_plain_bytes() {
        assert_eq!(parse_size_bytes("4096").unwrap(), 4096);
        assert_eq!(parse_size_bytes("0").unwrap(), 0);
    }

    #[test]
    fn parses_binary_suffixes() {
        assert_eq!(parse_size_bytes("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size_bytes("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size_bytes("1T").unwrap(), 1024_i64.pow(4));
        assert_eq!(parse_size_bytes("128K").unwrap(), 128 * 1024);
    }

    #[test]
    fn parses_iec_and_uppercase_spellings() {
        assert_eq!(parse_size_bytes("512MiB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size_bytes("512MB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size_bytes("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(
            parse_size_bytes("1.5G").unwrap(),
            (1.5 * 1024.0 * 1024.0 * 1024.0) as i64
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_size_bytes("").is_err());
        assert!(parse_size_bytes("M").is_err());
        assert!(parse_size_bytes("12X").is_err());
        assert!(parse_size_bytes("-2G").is_err());
        assert!(parse_size_bytes("abc").is_err());
    }

    #[test]
    fn storage_class_validation_matches_the_shared_vocabulary() {
        // The DP3 list the BFF validates against — every entry accepts.
        for class in chv_hypervisor_api::resources::BACKEND_CLASSES {
            assert!(validate_storage_class(class).is_ok(), "{class} must accept");
        }
        // Unknown classes reject client-side instead of a server 400…
        assert!(validate_storage_class("bogus").is_err());
        // …including the stord-boundary-only local aliases the BFF
        // rejects (#379: "local-file"/"localdisk" never accept here).
        assert!(validate_storage_class("localdisk").is_err());
        assert!(validate_storage_class("local-file").is_err());
        assert!(validate_storage_class("").is_err());
        // Whitespace: the validator itself is EXACT-match — the Create
        // arm trims the flag value before it reaches here and before it
        // goes on the wire (parity with the BFF, which also trims
        // before its vocabulary check), which is what makes that trim
        // load-bearing. The trim's end-to-end pin is the
        // vm_create_storage_class_row contract row, which passes
        // "local " (trailing space) and asserts the journaled boot
        // volume carries "local" — dropping the arm's trim flips that
        // row red (this exact-match rejection fires client-side).
        assert!(validate_storage_class(" lvm").is_err());
        assert!(validate_storage_class("local ").is_err());
    }

    #[test]
    fn cloud_init_userdata_passes_through_verbatim() {
        assert_eq!(
            resolve_cloud_init_userdata("#cloud-config\n").unwrap(),
            "#cloud-config\n"
        );
    }

    #[test]
    fn cloud_init_at_file_reads_the_file() {
        let dir = tempfile::tempdir().expect("cloud-init tempdir");
        let path = dir.path().join("user-data");
        std::fs::write(&path, "#cloud-config\nhostname: vm-1\n").expect("write user-data");
        let at_input = format!("@{}", path.display());
        assert_eq!(
            resolve_cloud_init_userdata(&at_input).unwrap(),
            "#cloud-config\nhostname: vm-1\n"
        );
    }

    #[test]
    fn cloud_init_at_missing_file_is_a_loud_error() {
        let err = resolve_cloud_init_userdata("@/nonexistent/chvctl/user-data").unwrap_err();
        assert!(err.to_string().contains("cloud-init file"));
    }

    #[test]
    fn import_file_reads_the_file() {
        let dir = tempfile::tempdir().expect("import tempdir");
        let path = dir.path().join("disk.qcow2");
        std::fs::write(&path, b"QFI\xfbrest-of-disk").expect("write disk image");
        assert_eq!(
            read_import_file(path.to_str().unwrap()).unwrap(),
            b"QFI\xfbrest-of-disk".to_vec()
        );
    }

    #[test]
    fn import_at_missing_file_is_a_loud_error() {
        let err = read_import_file("/nonexistent/chvctl/disk.qcow2").unwrap_err();
        assert!(err.to_string().contains("image file"));
    }

    #[test]
    fn import_at_oversize_file_is_refused_before_reading() {
        let dir = tempfile::tempdir().expect("import tempdir");
        let path = dir.path().join("huge.qcow2");
        // A sparse file: its metadata length is over the cap without
        // occupying disk, so the guard trips before any byte is read.
        std::fs::File::create(&path)
            .expect("create sparse file")
            .set_len(101 * 1024 * 1024 * 1024)
            .expect("set sparse length");
        let err = read_import_file(path.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("100 GiB"),
            "the oversize refusal must name the cap: {err}"
        );
    }

    #[test]
    fn import_at_non_regular_file_is_refused() {
        // /dev/null is a character device — readable-looking but not a
        // regular file; the guard must refuse it before any read.
        let err = read_import_file("/dev/null").unwrap_err();
        assert!(
            err.to_string().contains("not a regular file"),
            "the non-regular refusal must name it: {err}"
        );
    }
}
