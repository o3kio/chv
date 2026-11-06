#!/usr/bin/env python3
"""T0 architecture guards for the chv-agent/CellHV Core migration."""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path


FORBIDDEN_CORE_DEPENDENCIES = {
    "chv-controlplane-service",
    "chv-controlplane-store",
    "chv-controlplane-types",
    "chv-webui-bff",
    "chv-architecture-reconcile",
    "chv-architecture-validate",
}
CORE_MANIFESTS = (
    "cmd/chv-agent/Cargo.toml",
    "crates/chv-agent-core/Cargo.toml",
    "crates/cellhv-core-types/Cargo.toml",
    "crates/cellhv-core-store/Cargo.toml",
    "crates/cellhv-core-operations/Cargo.toml",
    "crates/cellhv-core-executor/Cargo.toml",
    "crates/cellhv-core-runtime-ownership/Cargo.toml",
    "crates/cellhv-core-runtime-owner/Cargo.toml",
    "crates/cellhv-core-recovery/Cargo.toml",
    "crates/cellhv-core-api/Cargo.toml",
    "crates/cellhv-nodecache-migration/Cargo.toml",
    "crates/cellhv-core-startup/Cargo.toml",
    "crates/cellhv-core-fs/Cargo.toml",
    "crates/chv-agent-runtime-ch/Cargo.toml",
)
ACTIVE_CODE_ROOTS = ("cmd", "crates", "gen", "proto", "packaging", "scripts")
TEXT_SUFFIXES = {".rs", ".toml", ".proto", ".yaml", ".yml", ".json", ".sh", ".service"}
ALLOWED_PACKAGED_SERVICES = {
    "chv-agent.service",
    "chv-controlplane.service",
    "chv-monitor-agent.service",
    "chv-nwd.service",
    "chv-stord.service",
}
PERSISTENT_LEASE_TOKEN = "cellhv-runtime-authority.lease"
FRESH_IDENTITY_SOURCE = "crates/cellhv-core-startup/src/identity.rs"
AUTHORITY_CLEANUP_PATHS = (
    "packaging/scripts",
    "packaging/systemd",
    "scripts/install.sh",
    "scripts/dev-install.sh",
)
DESTRUCTIVE_CLEANUP = re.compile(r"\b(?:rm|unlink)\b|\bfind\b.*\s-delete(?:\s|$)")
LEASE_GLOB = re.compile(r"(?=[^\n]*[*?])(?=[^\n]*(?:\.lease|runtime-authority|\.cellhv-))")
LEASE_PARENT_TARGET = re.compile(
    r"(?:\$\{?CHV_DATA_DIR\}?|/var/lib/chv)"
    r"(?:/agent)?(?:/(?:\.\*|\*))?(?=[\s'\"]|$)"
)
STORE_DEPENDENCIES = {"rusqlite", "sqlx", "libsqlite3-sys"}
CORE_STORE_PACKAGE = "cellhv-core-store"
CORE_OPERATIONS_PACKAGE = "cellhv-core-operations"
CORE_EXECUTOR_PACKAGE = "cellhv-core-executor"
CORE_RUNTIME_OWNERSHIP_PACKAGE = "cellhv-core-runtime-ownership"
CORE_RUNTIME_OWNER_PACKAGE = "cellhv-core-runtime-owner"
CORE_RECOVERY_PACKAGE = "cellhv-core-recovery"
LINUX_OBSERVATION_OWNER = Path("crates/chv-agent-runtime-ch")
LINUX_OBSERVATION_MODULE = Path("crates/chv-agent-runtime-ch/src/lib.rs")
AGENT_COMMAND_PACKAGE = "chv-agent"
OWNERSHIP_CAPABILITY_DEPENDENCIES = {
    CORE_RUNTIME_OWNERSHIP_PACKAGE,
    "procfs",
    "sysinfo",
}
OBSERVER_WIRING_TOKENS = re.compile(
    r"\b(?:LinuxOwnershipObservation|LinuxRuntimeObservation|LinuxObservation|"
    r"RuntimeObservation|linux_observation)\b|\bcellhv_core_runtime_ownership\b"
)
STORE_ALLOWED_CORE_DEPENDENCIES = {"cellhv-core-types"}
OPERATIONS_ALLOWED_DEPENDENCIES = {
    "async-channel",
    "cellhv-core-store",
    "cellhv-core-types",
    "serde",
    "serde_json",
    "thiserror",
    "tokio",
}
EXECUTOR_ALLOWED_DEPENDENCIES = {
    "async-trait",
    "cellhv-core-operations",
    "cellhv-core-types",
    "serde_json",
    "thiserror",
    "tokio",
    "uuid",
}
RUNTIME_OWNERSHIP_ALLOWED_DEPENDENCIES = {
    "cellhv-core-types",
    "libc",
    "serde",
    "serde_json",
    "thiserror",
}
RECOVERY_ALLOWED_DEPENDENCIES = {
    "cellhv-core-operations",
    "cellhv-core-runtime-ownership",
    "serde_json",
    "thiserror",
}
# The composition crate owns the executor, the Core API listener, and the
# authority actor: it may depend on the Core surface and tokio/tracing, and
# nothing else (no hypervisor runtime, no storage, no agent-plane crates —
# those belong behind the CoreVmRuntime/ActivatedStore traits it consumes).
RUNTIME_OWNER_ALLOWED_DEPENDENCIES = {
    "cellhv-core-api",
    "cellhv-core-executor",
    "cellhv-core-operations",
    "cellhv-core-startup",
    "thiserror",
    "tokio",
    "tracing",
}
OPERATION_AUTHORITY_DECLARATION = re.compile(
    r"\b(?:struct|enum|trait|type)\s+"
    r"(?:Operation(?:Engine|Service|Executor|Processor|Manager|Coordinator)|"
    r"OperationJournal|Journal(?:Engine|Service|Executor|Processor|Manager))\b"
)
OPERATION_MODULE_NAME = re.compile(
    r"(?:^|[_-])(?:operation(?:s|[_-](?:engine|service|executor|processor|manager))?|"
    r"journal(?:[_-](?:engine|service|executor|processor|manager))?)(?:$|[_-])"
)
EXECUTION_HANDLE_CAPABILITY = re.compile(r"\bExecutionHandle\b")
EXECUTION_CONSTRUCTOR_CAPABILITY = re.compile(r"\bspawn_with_execution\b")
EXECUTION_CAPABILITY_OWNER = Path("crates/cellhv-core-operations")
EXECUTION_HANDLE_CONSUMER = Path("crates/cellhv-core-executor")
EXECUTOR_TEST_SOURCE = Path("crates/cellhv-core-executor/src/tests.rs")
O3K_CONTRACT_PATH = "docs/acceptance/cellhv-o3k-core-client-contract-v1.json"
O3K_CONTRACT_SCHEMA_PATH = "docs/schemas/cellhv-o3k-core-client-contract-v1.schema.json"
O3K_PINNED_REVISION = "53fd2cb36ee79f42da49c8181d6ceed12b41b3aa"
O3K_SCENARIO_IDS = ("OCORE-001", "OCORE-002", "OCORE-003", "OCORE-004", "OCORE-005")


def load_json(path: Path) -> object:
    with path.open(encoding="utf-8") as stream:
        return json.load(stream)


def validate_required_object(value: object, schema: dict, location: str = "$") -> list[str]:
    """Validate the small JSON-Schema subset used by the Phase A registries."""
    errors: list[str] = []
    expected = schema.get("type")
    if expected == "object":
        if not isinstance(value, dict):
            return [f"{location}: expected object"]
        for key in schema.get("required", []):
            if key not in value:
                errors.append(f"{location}: missing required property {key!r}")
        if schema.get("additionalProperties") is False:
            allowed = set(schema.get("properties", {}))
            for key in value:
                if key not in allowed:
                    errors.append(f"{location}: unexpected property {key!r}")
        for key, child in schema.get("properties", {}).items():
            if key in value:
                errors.extend(validate_required_object(value[key], child, f"{location}.{key}"))
    elif expected == "array":
        if not isinstance(value, list):
            return [f"{location}: expected array"]
        if len(value) < schema.get("minItems", 0):
            errors.append(f"{location}: too few items")
        for index, item in enumerate(value):
            errors.extend(validate_required_object(item, schema.get("items", {}), f"{location}[{index}]"))
    elif expected == "string":
        if not isinstance(value, str):
            return [f"{location}: expected string"]
        if len(value) < schema.get("minLength", 0):
            errors.append(f"{location}: shorter than minLength")
        if "enum" in schema and value not in schema["enum"]:
            errors.append(f"{location}: {value!r} is not an allowed value")
        if "pattern" in schema and not re.fullmatch(schema["pattern"], value):
            errors.append(f"{location}: {value!r} does not match {schema['pattern']!r}")
    elif expected == "integer":
        if not isinstance(value, int) or isinstance(value, bool):
            errors.append(f"{location}: expected integer")
        elif value < schema.get("minimum", value):
            errors.append(f"{location}: below minimum")
    elif expected == "boolean" and not isinstance(value, bool):
        errors.append(f"{location}: expected boolean")
    return errors


def check(root: Path) -> list[str]:
    errors: list[str] = []
    manifests: dict[str, tuple[Path, dict]] = {}

    for manifest_path in root.glob("**/Cargo.toml"):
        if any(part in {"target", ".git"} for part in manifest_path.parts):
            continue
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        package = manifest.get("package", {})
        if package.get("name"):
            manifests[package["name"]] = (manifest_path, manifest)
        bins = manifest.get("bin", [])
        if package.get("name") == "cellhvd" or any(item.get("name") == "cellhvd" for item in bins):
            errors.append(f"{manifest_path.relative_to(root)}: parallel cellhvd binary is forbidden")

    packaged_services = {path.name for path in (root / "packaging/systemd").glob("*.service")}
    unexpected_services = sorted(packaged_services - ALLOWED_PACKAGED_SERVICES)
    if unexpected_services:
        errors.append(f"packaging/systemd: unclassified service(s): {', '.join(unexpected_services)}")
    agent_units = []
    for service in (root / "packaging/systemd").glob("*.service"):
        text = service.read_text(encoding="utf-8")
        if re.search(r"^ExecStart=.*(?:chv-agent|cellhv|core)", text, re.MULTILINE | re.IGNORECASE):
            agent_units.append(service.name)
    if agent_units != ["chv-agent.service"]:
        errors.append(f"packaging/systemd: exactly chv-agent.service must start the Core runtime; got {agent_units}")

    for relative in AUTHORITY_CLEANUP_PATHS:
        source = root / relative
        if not source.exists():
            continue
        paths = [source] if source.is_file() else source.rglob("*")
        for path in paths:
            if not path.is_file():
                continue
            for line_number, line in enumerate(
                path.read_text(encoding="utf-8", errors="replace").splitlines(), start=1
            ):
                if line.lstrip().startswith("#"):
                    continue
                if not DESTRUCTIVE_CLEANUP.search(line):
                    continue
                # dnsmasq keeps per-network DHCP leases under /run/chv/nwd and
                # those are runtime files that cleanup must remove. They are
                # not the Core runtime authority lease. Strip only dnsmasq
                # tokens so a line that also targets Core state still fails.
                checked = re.sub(r"\S*dnsmasq-\S*", "", line)
                if (
                    PERSISTENT_LEASE_TOKEN in checked
                    or LEASE_GLOB.search(checked)
                    or LEASE_PARENT_TARGET.search(checked)
                ):
                    errors.append(
                        f"{path.relative_to(root)}:{line_number}: package cleanup must preserve "
                        "the persistent Core runtime authority lease and its parent namespace"
                    )

    core_packages: set[str] = set()
    for name in manifests:
        if name == "chv-agent" or name.startswith("chv-agent-") or name.startswith("cellhv-core"):
            core_packages.add(name)
    dependency_graph: dict[str, set[str]] = {}
    store_packages: set[str] = set()
    for name, (_, manifest) in manifests.items():
        dependency_names: set[str] = set()
        dependency_tables = [manifest.get("dependencies", {}), manifest.get("build-dependencies", {})]
        for target in manifest.get("target", {}).values():
            dependency_tables.extend((target.get("dependencies", {}), target.get("build-dependencies", {})))
        for table in dependency_tables:
            for dependency_name, declaration in table.items():
                dependency_names.add(dependency_name)
                if isinstance(declaration, dict) and isinstance(declaration.get("package"), str):
                    dependency_names.add(declaration["package"])
        dependency_graph[name] = dependency_names
        if name in core_packages and dependency_names & STORE_DEPENDENCIES:
            store_packages.add(name)

    reachable = set(core_packages)
    pending = list(core_packages)
    referenced_dependencies: set[str] = set()
    while pending:
        dependencies = dependency_graph.get(pending.pop(), set())
        referenced_dependencies.update(dependencies)
        for dependency in dependencies:
            if dependency in manifests and dependency not in reachable:
                reachable.add(dependency)
                pending.append(dependency)
    forbidden_reachable = sorted((reachable | referenced_dependencies) & FORBIDDEN_CORE_DEPENDENCIES)
    if forbidden_reachable:
        errors.append(f"Core dependency graph reaches forbidden package(s): {', '.join(forbidden_reachable)}")

    store_dependents = sorted(
        name for name, dependencies in dependency_graph.items() if CORE_STORE_PACKAGE in dependencies
    )
    unexpected_store_dependents = [name for name in store_dependents if name != CORE_OPERATIONS_PACKAGE]
    if unexpected_store_dependents:
        errors.append(
            f"{CORE_STORE_PACKAGE}: may only be depended on by {CORE_OPERATIONS_PACKAGE}; "
            f"bypassed by {unexpected_store_dependents}"
        )
    if CORE_OPERATIONS_PACKAGE in manifests and CORE_STORE_PACKAGE not in dependency_graph.get(
        CORE_OPERATIONS_PACKAGE, set()
    ):
        errors.append(f"{CORE_OPERATIONS_PACKAGE}: must directly depend on {CORE_STORE_PACKAGE}")
    unexpected_operations_dependencies = sorted(
        dependency_graph.get(CORE_OPERATIONS_PACKAGE, set()) - OPERATIONS_ALLOWED_DEPENDENCIES
    )
    if unexpected_operations_dependencies:
        errors.append(
            f"{CORE_OPERATIONS_PACKAGE}: dependency boundary forbids "
            f"{unexpected_operations_dependencies}"
        )
    unexpected_executor_dependencies = sorted(
        dependency_graph.get(CORE_EXECUTOR_PACKAGE, set()) - EXECUTOR_ALLOWED_DEPENDENCIES
    )
    if unexpected_executor_dependencies:
        errors.append(
            f"{CORE_EXECUTOR_PACKAGE}: dependency boundary forbids "
            f"{unexpected_executor_dependencies}"
        )
    unexpected_runtime_owner_dependencies = sorted(
        dependency_graph.get(CORE_RUNTIME_OWNER_PACKAGE, set())
        - RUNTIME_OWNER_ALLOWED_DEPENDENCIES
    )
    if unexpected_runtime_owner_dependencies:
        errors.append(
            f"{CORE_RUNTIME_OWNER_PACKAGE}: dependency boundary forbids "
            f"{unexpected_runtime_owner_dependencies}"
        )
    unexpected_ownership_dependencies = sorted(
        dependency_graph.get(CORE_RUNTIME_OWNERSHIP_PACKAGE, set())
        - RUNTIME_OWNERSHIP_ALLOWED_DEPENDENCIES
    )
    if unexpected_ownership_dependencies:
        errors.append(
            f"{CORE_RUNTIME_OWNERSHIP_PACKAGE}: dependency boundary forbids "
            f"{unexpected_ownership_dependencies}"
        )
    unexpected_recovery_dependencies = sorted(
        dependency_graph.get(CORE_RECOVERY_PACKAGE, set()) - RECOVERY_ALLOWED_DEPENDENCIES
    )
    if unexpected_recovery_dependencies:
        errors.append(
            f"{CORE_RECOVERY_PACKAGE}: dependency boundary forbids "
            f"{unexpected_recovery_dependencies}"
        )
    recovery_dependents = sorted(
        name for name, dependencies in dependency_graph.items() if CORE_RECOVERY_PACKAGE in dependencies
    )
    if recovery_dependents:
        errors.append(
            f"{CORE_RECOVERY_PACKAGE}: assessment-only recovery must remain production-unwired; "
            f"depended on by {recovery_dependents}"
        )
    for relative in (
        Path("crates/cellhv-core-store/src/lib.rs"),
        Path("crates/cellhv-core-operations/src/lib.rs"),
    ):
        path = root / relative
        if not path.exists():
            continue
        source = path.read_text(encoding="utf-8")
        if re.search(r"\bpub\s+active_attempt_token\s*:", source):
            errors.append(
                f"{relative}: recovery and restart projections must not expose attempt tokens"
            )

    # Process identity is a runtime-backend capability. Transports,
    # compatibility adapters, and provider daemons must consume Core results,
    # never inspect Linux process ownership themselves.
    for name in dependency_graph:
        is_restricted_consumer = (
            name == "cellhv-core-api"
            or "compat" in name
            or name.startswith("chv-nwd")
            or name.startswith("chv-stord")
            or "provider" in name
        )
        consumer_reachable: set[str] = set()
        consumer_pending = [name]
        while consumer_pending:
            package = consumer_pending.pop()
            for dependency in dependency_graph.get(package, set()):
                if dependency not in consumer_reachable:
                    consumer_reachable.add(dependency)
                    if dependency in manifests:
                        consumer_pending.append(dependency)
        escaped = sorted(consumer_reachable & OWNERSHIP_CAPABILITY_DEPENDENCIES)
        if is_restricted_consumer and escaped:
            errors.append(
                f"{name}: transport/compatibility/provider boundary forbids "
                f"process-ownership dependencies {escaped}"
            )

    agent_dependencies = dependency_graph.get(AGENT_COMMAND_PACKAGE, set())
    if CORE_RUNTIME_OWNERSHIP_PACKAGE in agent_dependencies:
        errors.append(
            "cmd/chv-agent: production observer is not approved for composition; "
            "runtime ownership must remain behind chv-agent-runtime-ch"
        )

    for source_root in (root / "cmd", root / "crates"):
        if not source_root.exists():
            continue
        for source in source_root.rglob("*.rs"):
            relative = source.relative_to(root)
            text = source.read_text(encoding="utf-8", errors="replace")
            observation_names = {"Observation"}
            observation_names.update(
                re.findall(
                    r"\buse\s+[^;]*\bObservation\s+as\s+([A-Za-z_][A-Za-z0-9_]*)\s*;",
                    text,
                )
            )
            implements_observation = any(
                re.search(
                    r"\bimpl(?:\s*<[^>]*>)?\s+(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*"
                    + re.escape(name)
                    + r"\s+for\b",
                    text,
                )
                for name in observation_names
            )
            if implements_observation:
                is_test_fixture = (
                    relative.is_relative_to(Path("crates/cellhv-core-runtime-ownership"))
                    and (source.name == "tests.rs" or "#[cfg(test)]" in text)
                )
                if not relative.is_relative_to(LINUX_OBSERVATION_OWNER) and not is_test_fixture:
                    errors.append(
                        f"{relative}: production Linux Observation implementation is restricted "
                        f"to {LINUX_OBSERVATION_OWNER}"
                    )

    observation_module = root / LINUX_OBSERVATION_MODULE
    try:
        observation_module_text = observation_module.read_text(encoding="utf-8")
    except OSError as exc:
        errors.append(f"{LINUX_OBSERVATION_MODULE}: {exc}")
    else:
        if not re.search(r"(?m)^pub\(crate\) mod linux_observation;$", observation_module_text):
            errors.append(
                f"{LINUX_OBSERVATION_MODULE}: linux_observation must remain crate-private "
                "until the Phase C recovery transition is approved"
            )
        if not re.search(
            r"(?m)^#\[allow\(dead_code\)\]\s*\n"
            r"^pub\(crate\) mod linux_observation;$",
            observation_module_text,
        ):
            errors.append(
                f"{LINUX_OBSERVATION_MODULE}: the unwired observer must carry the single "
                "module-level dead_code allowance until production composition removes it"
            )

    agent_source_root = root / "cmd/chv-agent/src"
    if agent_source_root.exists():
        for agent_source in agent_source_root.rglob("*.rs"):
            if OBSERVER_WIRING_TOKENS.search(
                agent_source.read_text(encoding="utf-8", errors="replace")
            ):
                errors.append(
                    f"{agent_source.relative_to(root)}: production ownership observer wiring is "
                    "forbidden until the Phase C recovery transition is approved"
                )

    for source_root in (root / "cmd", root / "crates"):
        if not source_root.exists():
            continue
        for source in source_root.rglob("*.rs"):
            relative = source.relative_to(root)
            if relative.is_relative_to(EXECUTION_CAPABILITY_OWNER):
                continue
            if relative == EXECUTOR_TEST_SOURCE:
                continue
            text = source.read_text(encoding="utf-8", errors="replace")
            is_executor = relative.is_relative_to(EXECUTION_HANDLE_CONSUMER)
            production_text = text
            if is_executor and re.search(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub\s+)?mod\s+\w+\s*\{", text):
                errors.append(f"{relative}: inline cfg(test) modules are forbidden in executor source")
            forbidden = bool(EXECUTION_CONSTRUCTOR_CAPABILITY.search(production_text)) or (
                bool(EXECUTION_HANDLE_CAPABILITY.search(production_text)) and not is_executor
            )
            if forbidden:
                errors.append(
                    f"{relative}: execution capability is restricted to "
                    f"{EXECUTION_CAPABILITY_OWNER} or {EXECUTION_HANDLE_CONSUMER}"
                )

    api_source_root = root / "crates/cellhv-core-api/src"
    api_sources = sorted(api_source_root.rglob("*.rs")) if api_source_root.exists() else []
    production_api_sources = [path for path in api_sources if path.name != "tests.rs"]
    combined_api_source = "\n".join(
        path.read_text(encoding="utf-8", errors="replace") for path in production_api_sources
    )
    router_declarations = re.findall(
        r"pub\s+fn\s+router\s*\((.*?)\)\s*(?:->|where|\{)",
        combined_api_source,
        re.DOTALL,
    )
    if not router_declarations or not any(
        re.search(
            r"\b[A-Za-z_][A-Za-z0-9_]*\s*:\s*"
            r"(?:cellhv_core_operations\s*::\s*)?AuthorityHandle\b",
            parameters,
        )
        for parameters in router_declarations
    ):
        errors.append("crates/cellhv-core-api: router must receive the shared AuthorityHandle")
    for source in api_sources:
        text = source.read_text(encoding="utf-8", errors="replace")
        relative = source.relative_to(root)
        if re.search(r"\b(?:struct|enum|type)\s+DbActor\b", text):
            errors.append(f"{relative}: private DbActor is forbidden")
    for source in production_api_sources:
        text = source.read_text(encoding="utf-8", errors="replace")
        relative = source.relative_to(root)
        if re.search(r"\bOperationService\b", text):
            errors.append(
                f"{relative}: transport must not construct, alias, or own OperationService"
            )

    api_manifest_entry = manifests.get("cellhv-core-api")
    if api_manifest_entry is not None:
        _, api_manifest = api_manifest_entry
        api_dependency_tables = [
            api_manifest.get("dependencies", {}),
            api_manifest.get("dev-dependencies", {}),
            api_manifest.get("build-dependencies", {}),
        ]
        for target in api_manifest.get("target", {}).values():
            api_dependency_tables.extend(
                (
                    target.get("dependencies", {}),
                    target.get("dev-dependencies", {}),
                    target.get("build-dependencies", {}),
                )
            )
        api_dependency_packages = set()
        for table in api_dependency_tables:
            for dependency_name, declaration in table.items():
                api_dependency_packages.add(dependency_name)
                if isinstance(declaration, dict) and isinstance(declaration.get("package"), str):
                    api_dependency_packages.add(declaration["package"])
        if "chv-agent-core" in api_dependency_packages:
            errors.append(
                "crates/cellhv-core-api: dependency direction forbids chv-agent-core "
                "in normal, build, target, and dev dependencies"
            )

    store_core_dependencies = dependency_graph.get(CORE_STORE_PACKAGE, set()) & core_packages
    unexpected_store_dependencies = sorted(store_core_dependencies - STORE_ALLOWED_CORE_DEPENDENCIES)
    if unexpected_store_dependencies:
        errors.append(
            f"{CORE_STORE_PACKAGE}: forbidden Core dependency(s) {unexpected_store_dependencies}; "
            f"only {sorted(STORE_ALLOWED_CORE_DEPENDENCIES)} are allowed"
        )

    for relative in CORE_MANIFESTS:
        path = root / relative
        if not path.exists():
            errors.append(f"{relative}: required Core manifest is missing")
            continue
        manifest = tomllib.loads(path.read_text(encoding="utf-8"))

    authority_path = root / "config/cellhv-core-authority-v1.json"
    try:
        authority = load_json(authority_path)
    except (OSError, ValueError) as exc:
        errors.append(f"config/cellhv-core-authority-v1.json: {exc}")
    else:
        expected = {
            "runtime_service": "chv-agent",
            "runtime_binary": "chv-agent",
            "vm_authority_count": 1,
            "durable_vm_store_count": 1,
            "operation_engine_count": 1,
            "vmm_backend": "cloud-hypervisor",
            "qemu_identity": False,
        }
        for key, expected_value in expected.items():
            if authority.get(key) != expected_value:
                errors.append(f"config/cellhv-core-authority-v1.json: {key} must be {expected_value!r}")
        if authority.get("durable_vm_store_count") != len(store_packages):
            errors.append(
                "config/cellhv-core-authority-v1.json: durable_vm_store_count does not match "
                f"store-bearing Core packages {sorted(store_packages)}"
            )

        operation_engines = [
            manifests[name][0].parent.relative_to(root).as_posix()
            for name in core_packages
            if name == CORE_OPERATIONS_PACKAGE
        ]
        escaped_operation_authorities = []
        process_owners = []
        for name in core_packages:
            package_root = manifests[name][0].parent
            if name != CORE_OPERATIONS_PACKAGE and (
                OPERATION_MODULE_NAME.search(name) or OPERATION_MODULE_NAME.search(package_root.name)
            ):
                escaped_operation_authorities.append(package_root.relative_to(root).as_posix())
            for source in package_root.glob("src/**/*.rs"):
                text = source.read_text(encoding="utf-8", errors="replace")
                if name == CORE_EXECUTOR_PACKAGE:
                    if source.relative_to(root) == EXECUTOR_TEST_SOURCE:
                        continue
                    production = text
                    remaining = re.sub(r"\bpub\s+struct\s+JournalExecutor\b", "", production)
                    if OPERATION_AUTHORITY_DECLARATION.search(remaining):
                        escaped_operation_authorities.append(source.relative_to(root).as_posix())
                elif name != CORE_OPERATIONS_PACKAGE and (
                    OPERATION_MODULE_NAME.search(source.stem)
                    or OPERATION_AUTHORITY_DECLARATION.search(text)
                ):
                    escaped_operation_authorities.append(source.relative_to(root).as_posix())
                if re.search(r"HashMap\s*<\s*String\s*,\s*VmProcess\s*>", text):
                    process_owners.append(source.relative_to(root).as_posix())
        if authority.get("operation_engine_count") != len(operation_engines):
            errors.append(
                "config/cellhv-core-authority-v1.json: operation_engine_count does not match "
                f"operation engines {operation_engines}"
            )
        if escaped_operation_authorities:
            errors.append(
                f"operation authority must be owned by {CORE_OPERATIONS_PACKAGE}; found "
                f"{sorted(set(escaped_operation_authorities))}"
            )
        if authority.get("vm_process_owner_count") != len(process_owners):
            errors.append(
                "config/cellhv-core-authority-v1.json: vm_process_owner_count does not match "
                f"process owners {process_owners}"
            )

    fresh_identity_path = root / FRESH_IDENTITY_SOURCE
    try:
        fresh_identity = fresh_identity_path.read_text(encoding="utf-8")
    except OSError as exc:
        errors.append(f"{FRESH_IDENTITY_SOURCE}: {exc}")
    else:
        opaque_fresh = re.search(
            r"pub struct FreshHostIdentity\s*\{(?P<body>.*?)\}",
            fresh_identity,
            re.DOTALL,
        )
        if (
            opaque_fresh is None
            or re.search(r"\bpub\s+(?:identity|source)\s*:", opaque_fresh.group("body"))
            or "InitializeFresh(FreshHostIdentity)" not in fresh_identity
        ):
            errors.append(
                f"{FRESH_IDENTITY_SOURCE}: fresh identity authorization must be an "
                "opaque resolver-issued tuple payload"
            )

    identity_policy_path = root / "config/cellhv-core-identity-policy-v1.json"
    try:
        identity_policy = load_json(identity_policy_path)
    except (OSError, ValueError) as exc:
        errors.append(f"config/cellhv-core-identity-policy-v1.json: {exc}")
    else:
        expected_identity_policy = {
            "version": 1,
            "adr_status": "proposed",
            "production_startup_enforced": False,
            "host_identity_resolver_enforced": True,
            "fresh_store_initializer_enforced": True,
            "importer_reserved_host_ids_enforced": True,
            "nodecache_authority_facade_enforced": False,
            "nodecache_authority_mode_enforced": False,
            "durable_identity_precedence": [
                "existing-core-database",
                "importable-nodecache",
                "configured-fresh-seed",
                "identity-preserving-precreation-enrollment",
                "persisted-one-time-uuid",
            ],
            "reserved_host_ids": ["unknown", "unset", "none", "null"],
            "machine_id_is_host_id_source": False,
            "configured_node_id_can_override_durable_identity": False,
            "enrollment_can_replace_core_host_id": False,
            "unreadable_database_can_be_reinitialized": False,
            "nodecache_authority_modes": [
                "legacy-vm-authority",
                "core-vm-authority",
                "blocked",
            ],
            "post_cutover_vm_writable_store": "core-sqlite-only",
        }
        if identity_policy != expected_identity_policy:
            errors.append(
                "config/cellhv-core-identity-policy-v1.json: policy must exactly match ADR-019"
            )

    identity_adr_path = root / "docs/specs/adr/019-stable-core-host-identity-and-nodecache-authority.md"
    try:
        identity_adr = identity_adr_path.read_text(encoding="utf-8")
    except OSError as exc:
        errors.append(f"{identity_adr_path.relative_to(root)}: {exc}")
    else:
        status = re.search(r"^## Status\s+^(\S+)\s*$", identity_adr, re.MULTILINE)
        if status is None or status.group(1) != "Proposed":
            errors.append(
                "docs/specs/adr/019-stable-core-host-identity-and-nodecache-authority.md: "
                "status must remain Proposed until production startup and cache modes are enforced"
            )

    forbidden_identity = re.compile(r"\bqmp\b|qemu:///system|\bqemu(?!-(?:img|utils|kvm))\b", re.IGNORECASE)
    for directory in ACTIVE_CODE_ROOTS:
        base = root / directory
        if not base.exists():
            continue
        for path in base.rglob("*"):
            if not path.is_file() or path.suffix not in TEXT_SUFFIXES:
                continue
            if path.name == Path(__file__).name:
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            if forbidden_identity.search(text):
                errors.append(f"{path.relative_to(root)}: forbidden Cloud Hypervisor QEMU/QMP identity")

    try:
        o3k_contract = load_json(root / O3K_CONTRACT_PATH)
    except (OSError, ValueError) as exc:
        errors.append(f"{O3K_CONTRACT_PATH}: {exc}")
    else:
        expected_fields = {
            "schema_version": 1,
            "profile": "o3k-core-client-t1",
            "evidence_status": "proposed-not-run",
            "result": "not-run",
            "maximum_evidence_tier": "T1",
            "t5_eligible": False,
            "o3k_repository": "https://github.com/kubedoio/o3k",
            "o3k_revision": O3K_PINNED_REVISION,
            "o3k_module": "github.com/cobaltcore-dev/o3k",
            "core_api_contract": "crates/cellhv-core-api/contract/cellhv-core-api-v1.json",
            "scenarios": list(O3K_SCENARIO_IDS),
            "executed_scenarios": [],
        }
        if not isinstance(o3k_contract, dict) or any(
            o3k_contract.get(key) != value for key, value in expected_fields.items()
        ):
            errors.append(
                f"{O3K_CONTRACT_PATH}: pinned profile must remain not-run, T1-only, "
                "and fixed to the audited O3K revision"
            )

    registry_path = root / "docs/acceptance/cellhv-core-registry-v1.json"
    try:
        registry = load_json(registry_path)
        scenarios = registry.get("scenarios", []) if isinstance(registry, dict) else []
        o3k_scenarios = {
            scenario.get("id"): scenario
            for scenario in scenarios
            if isinstance(scenario, dict) and str(scenario.get("id", "")).startswith("OCORE-")
        }
    except (OSError, ValueError) as exc:
        errors.append(f"docs/acceptance/cellhv-core-registry-v1.json: {exc}")
    else:
        if set(o3k_scenarios) != set(O3K_SCENARIO_IDS) or any(
            scenario.get("tier") != "T1" for scenario in o3k_scenarios.values()
        ):
            errors.append(
                "docs/acceptance/cellhv-core-registry-v1.json: O3K Core client "
                "scenarios must be exactly the registered T1-only set and may never be T5"
            )

    validations = (
        ("docs/acceptance/cellhv-core-registry-v1.json", "docs/schemas/cellhv-acceptance-registry-v1.schema.json"),
        (O3K_CONTRACT_PATH, O3K_CONTRACT_SCHEMA_PATH),
        ("docs/qualification/cellhv-core-phase-a-claim.json", "docs/schemas/cellhv-compatibility-claim-v1.schema.json"),
    )
    for document_name, schema_name in validations:
        try:
            document = load_json(root / document_name)
            schema = load_json(root / schema_name)
            errors.extend(f"{document_name}: {error}" for error in validate_required_object(document, schema))
        except (OSError, ValueError) as exc:
            errors.append(f"{document_name}: {exc}")

    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    errors = check(args.root.resolve())
    if errors:
        print("CellHV Core architecture guard failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print("CellHV Core architecture and registry guards passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
