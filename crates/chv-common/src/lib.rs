use std::time::{SystemTime, UNIX_EPOCH};

pub const OPERATION_ID_METADATA_KEY: &str = "x-operation-id";

/// Audience bound into every short-lived console ticket (#660, #662).
///
/// Domain separation between the console endpoint and the rest of the API:
/// a session JWT carries `chv:session` and is rejected by the console
/// validator, while a console ticket is rejected by the session extractor.
/// Defined once so the BFF mint path and the agent validator cannot drift.
pub const CONSOLE_AUDIENCE: &str = "chv:console";

/// Application-layer credential the control plane presents on the
/// control-plane → agent hop. The transport is a local Unix-domain socket
/// (0600); this metadata key carries the control plane's agent bearer
/// token, which the agent validates before any handler runs. See
/// `docs/ARCHITECTURE.md` and
/// `docs/runbooks/agent-auth-and-secret-rotation.md`.
pub const NODE_AUTH_METADATA_KEY: &str = "x-chv-agent-token";

/// Constant-time byte comparison for bearer credentials.
///
/// Length is not secret (tokens have a fixed configured size), but the
/// content comparison must not short-circuit on the first differing byte.
/// Used by the agent's inbound authentication interceptor.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

pub mod clock;
pub mod firewall;
pub mod hypervisor;
pub mod migration_auth;

pub use clock::{Clock, ManualClock, SystemClock};

pub mod types {
    use std::collections::HashMap;

    #[derive(Debug, Clone)]
    pub struct BackendLocator {
        pub backend_class: String,
        pub locator: String,
        pub options: HashMap<String, String>,
    }

    #[derive(Debug, Clone, Default)]
    pub struct DevicePolicy {
        pub read_bps: u64,
        pub write_bps: u64,
        pub read_iops: u64,
        pub write_iops: u64,
        pub burst_allowed: bool,
        pub read_only: bool,
        pub no_exec: bool,
        pub io_scheduler: String,
        pub cache_mode: String,
    }
}

pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_millis() as i64
}

/// Generate a 128-bit lowercase hex resource ID (32 characters).
///
/// This is the canonical identifier for persistent resources (VMs,
/// volumes, networks, images, users, tokens, snapshots, and architecture
/// objects). 128 bits keeps birthday-collision pressure negligible at any
/// realistic fleet size (#664).
pub fn gen_resource_id() -> String {
    use rand::RngExt;
    let bytes: [u8; 16] = rand::rng().random();
    hex::encode(bytes)
}

/// Generate a short 8-character lowercase hex ID (e.g. "3f7a2bc1").
///
/// Legacy helper retained only for values whose format is externally
/// pinned: CHV certificate serials (exactly eight lowercase hex characters,
/// see `CHV_CERTIFICATE_SERIAL_HEX_LEN`) and serial components that must
/// stay eight characters (monitoring certificates concatenate two of them).
/// Do NOT use it for persistent resource identifiers — use
/// [`gen_resource_id`] instead.
pub fn gen_short_id() -> String {
    use rand::RngExt;
    let bytes: [u8; 4] = rand::rng().random();
    hex::encode(bytes)
}

/// Compute SHA-256 of `input` and return it as a lowercase hex string.
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Compute SHA-256 of raw bytes and return it as a lowercase hex
/// string (certificate fingerprints and similar DER digests).
pub fn sha256_hex_bytes(input: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input);
    hex::encode(hasher.finalize())
}

/// Compute FNV-1a hash for a string input.
pub fn fnv1a_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    hash
}

/// Is a firewall policy snapshot semantically EMPTY (#355)? True for a
/// blank string or a JSON array with only whitespace between the
/// brackets (hand-rolled to keep this crate serde-free). Empty policies
/// are never applied: nwd's engine engages default-deny even for an
/// empty ruleset, which would cut a rule-less network's guests off
/// entirely (including DHCP). Anything else — including unparseable
/// text — is NOT empty: it flows to nwd, whose validation fails loudly
/// on garbage instead of silently dropping the operator's config.
pub fn firewall_ruleset_is_empty(policy_json: &str) -> bool {
    let trimmed = policy_json.trim();
    if trimmed.is_empty() {
        return true;
    }
    match trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(inner) => inner.chars().all(char::is_whitespace),
        None => false,
    }
}

/// Validate that `id` contains only lowercase hex characters (a-f, 0-9).
/// Returns `true` if the id is non-empty and matches `^[a-f0-9]+$`.
pub fn validate_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// Validate that `id` is safe to use as a single filesystem path component:
/// non-empty, containing only ASCII alphanumerics plus `.`, `_`, `-`, and
/// never `..` (so `/`, `\`, and path traversal are rejected).
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !id.contains("..")
}

/// Validate that `component` is safe to join under a fixed directory root:
/// non-empty, not `.` or `..`, no separators, and no control characters.
/// Unlike [`is_safe_id`] this permits arbitrary printable characters —
/// e.g. `:` in tag-style names such as `image:latest` — rejecting exactly
/// the path-traversal vectors (separators and dot components) so that
/// names accepted before the boundary check existed remain valid.
pub fn is_safe_path_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

/// Construct a bridge name for a network, guaranteed to be <= 15 chars (IFNAMSIZ limit).
///
/// For the "default" network, returns "chvbr0". For other networks, returns
/// "br-{net_id}" if it fits in 15 chars, otherwise truncates net_id and appends
/// a 4-hex-char hash suffix to avoid collisions: "br-{prefix}{hash}".
///
/// Relocated from `crates/chv-agent-core/src/reconcile.rs` (M2.2a) and then
/// from `crates/chv-hypervisor-api/src/resources.rs` (#356 N5): the bridge
/// name is now derived by the agent runtime, the legacy reconcile path, AND
/// nwd's no-state local-teardown fallback — one definition for all three.
pub fn bridge_name_for_network(net_id: &str) -> String {
    if net_id == "default" {
        return "chvbr0".to_string();
    }
    let candidate = format!("br-{}", net_id);
    if candidate.len() <= 15 {
        return candidate;
    }
    // "br-" (3) + up to 8 chars of net_id + 4-char hash = 15 chars total
    let prefix: String = net_id.chars().take(8).collect();
    let hash = {
        let mut h: u32 = 0x811c9dc5;
        for b in net_id.as_bytes() {
            h = h.wrapping_mul(0x01000193) ^ (*b as u32);
        }
        format!("{:04x}", h & 0xffff)
    };
    format!("br-{}{}", prefix, hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_name_matches_the_shell_removal_helper_vectors() {
        // The removal scripts reimplement this function in POSIX sh; these
        // vectors are the shared contract (see
        // tests/test_cellhv_core_packaging.py).
        assert_eq!(bridge_name_for_network("default"), "chvbr0");
        assert_eq!(bridge_name_for_network("net1"), "br-net1");
        assert_eq!(
            bridge_name_for_network("63b27631c9c92fda4a43351c5aeeba21"),
            "br-63b276315a70"
        );
        // IFNAMSIZ guard: even a long id stays within 15 chars and keeps
        // the 8-char prefix + 4-hex FNV-1a suffix.
        let long = "a".repeat(64);
        let name = bridge_name_for_network(&long);
        assert_eq!(name.len(), 15);
        assert!(name.starts_with("br-aaaaaaaa"));
    }

    #[test]
    fn firewall_ruleset_is_empty_matches_blank_and_empty_arrays() {
        // #355: blank strings and empty JSON arrays are semantically
        // empty — they must never engage default-deny-with-no-allows.
        for empty in ["", "   ", "\n", "[]", "[ ]", "[\n\t]"] {
            assert!(
                firewall_ruleset_is_empty(empty),
                "{empty:?} must be treated as empty"
            );
        }
        // Real rules, whitespace-only strings inside brackets, non-array
        // shapes, and garbage are NOT empty — they flow to nwd, whose
        // validation fails loudly on garbage instead of silently dropping
        // the operator's config.
        for non_empty in [
            r#"[{"direction":"inbound","action":"accept","protocol":"icmp"}]"#,
            "[{}]",
            "null",
            "{",
            "allow-all",
        ] {
            assert!(
                !firewall_ruleset_is_empty(non_empty),
                "{non_empty:?} must NOT be treated as empty"
            );
        }
    }

    #[test]
    fn gen_short_id_is_8_hex_chars() {
        let id = gen_short_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn gen_short_id_uniqueness() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| gen_short_id()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn gen_resource_id_is_128_bits_of_hex() {
        let id = gen_resource_id();
        assert_eq!(id.len(), 32, "128 bits encode to 32 hex characters");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(validate_id(&id), "resource ids must satisfy validate_id");
    }

    #[test]
    fn gen_resource_id_uniqueness() {
        let ids: std::collections::HashSet<String> =
            (0..10_000).map(|_| gen_resource_id()).collect();
        assert_eq!(ids.len(), 10_000);
    }

    #[test]
    fn sha256_hex_produces_64_char_hex_string() {
        let hash = sha256_hex("chv_test_token");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        assert_eq!(sha256_hex("same"), sha256_hex("same"));
    }

    #[test]
    fn validate_id_accepts_lowercase_hex() {
        assert!(validate_id("3f7a2bc1"));
        assert!(validate_id("0000ffff"));
        assert!(validate_id("abcdef01"));
    }

    #[test]
    fn validate_id_rejects_invalid() {
        assert!(!validate_id(""));
        assert!(!validate_id("ABCDEF")); // uppercase
        assert!(!validate_id("../etc/passwd")); // path traversal
        assert!(!validate_id("abc xyz")); // space
        assert!(!validate_id("g1h2i3j4")); // non-hex letters
    }

    #[test]
    fn is_safe_id_accepts_safe_ids() {
        assert!(is_safe_id("vm-1"));
        assert!(is_safe_id("VM_2.test"));
        assert!(is_safe_id("vol.3-x"));
    }

    #[test]
    fn is_safe_id_rejects_unsafe_ids() {
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id("a\\b"));
        assert!(!is_safe_id(".."));
        assert!(!is_safe_id("a..b")); // path traversal
        assert!(!is_safe_id("a b"));
        assert!(!is_safe_id("aéb")); // non-ASCII
    }

    #[test]
    fn is_safe_path_component_accepts_tag_style_names() {
        // Compatibility class: names that were valid image_ref values before
        // the trust-boundary check existed must keep working.
        assert!(is_safe_path_component("test-image:latest"));
        assert!(is_safe_path_component("ubuntu-22.04.qcow2"));
        assert!(is_safe_path_component("vm image #1")); // spaces are legal in a component
        assert!(is_safe_path_component("a..b")); // literal name, cannot traverse without a separator
    }

    #[test]
    fn is_safe_path_component_rejects_traversal_vectors() {
        assert!(!is_safe_path_component(""));
        assert!(!is_safe_path_component("."));
        assert!(!is_safe_path_component(".."));
        assert!(!is_safe_path_component("a/b"));
        assert!(!is_safe_path_component("a\\b"));
        assert!(!is_safe_path_component("sub/dir/name"));
        assert!(!is_safe_path_component("a\nb")); // control characters
        assert!(!is_safe_path_component("a\0b"));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentOwnership {
    pub vm_id: String,
    pub operation_id: Option<String>,
    pub requester: Option<String>,
}
