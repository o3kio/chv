import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]


class CellHvCorePackagingTests(unittest.TestCase):
    def test_shipped_agent_defaults_are_explicit_and_disabled_by_wiring(self):
        for relative in ("docs/examples/agent.toml", "packaging/config/chv.yaml"):
            text = (ROOT / relative).read_text()
            self.assertIn("core_store_path", text)
            self.assertIn("/var/lib/chv/agent/core.db", text)
            self.assertIn("core_api_socket_path", text)
            self.assertIn("/run/chv/core/core-v1.sock", text)
        policy = (ROOT / "config/cellhv-core-identity-policy-v1.json").read_text()
        self.assertIn('"production_startup_enforced": false', policy)

    def test_agent_unit_has_private_systemd_parents_and_unprivileged_identity(self):
        unit = (ROOT / "packaging/systemd/chv-agent.service").read_text()
        self.assertIn("User=chv", unit)
        self.assertIn("Group=chv", unit)
        self.assertIn("UMask=002", unit)
        self.assertIn("RuntimeDirectory=chv/agent", unit)
        self.assertIn("RuntimeDirectoryMode=0775", unit)
        self.assertIn("StateDirectoryMode=0700", unit)
        self.assertIn("ExecStartPre=+/usr/bin/install -d -m 0700", unit)
        self.assertNotIn("ExecStartPost=", unit)
        self.assertIn("ExecStopPost=-/bin/rm -f /run/chv/agent/api.sock", unit)
        self.assertNotRegex(unit, r"rm\s+-f\s+/run/chv/core/core-v1\.sock")

    def test_agent_unit_restart_preserves_vms_and_sibling_runtime_dirs(self):
        # Field-qualification blocker: `KillMode=mixed` SIGKILLs the whole
        # cgroup when the main process exits (killing every running VMM),
        # and `RuntimeDirectory=chv` made systemd remove /run/chv
        # recursively, unlinking the separately-managed chv-stord/chv-nwd
        # sockets. VMs must outlive the agent and the supervisor must be
        # able to adopt the living daemons after a restart.
        unit = (ROOT / "packaging/systemd/chv-agent.service").read_text()
        self.assertIn("KillMode=process", unit)
        self.assertNotIn("KillMode=mixed", unit)
        self.assertIn("RuntimeDirectory=chv/agent", unit)
        self.assertNotRegex(unit, r"(?m)^RuntimeDirectory=chv$")
        self.assertIn("TimeoutStopSec=75", unit)

    def test_standalone_and_packaged_agent_units_have_security_parity(self):
        # PR #334 ("install.sh ships canonical units") removed the embedded
        # unit heredoc this test originally parsed. install.sh now installs
        # the canonical unit files verbatim: from the release tarball's
        # systemd/ (populated from docs/examples/systemd/ by
        # scripts/build-release.sh) or, from a repository checkout,
        # packaging/systemd/ itself. Standalone-vs-packaged parity is
        # therefore between the .deb unit (packaging/systemd/) and the
        # standalone-install unit source (docs/examples/systemd/).
        packaged = (ROOT / "packaging/systemd/chv-agent.service").read_text()
        standalone = (ROOT / "docs/examples/systemd/chv-agent.service").read_text()
        installer = (ROOT / "scripts/install.sh").read_text()
        # Parity is structural: install.sh must keep installing the
        # canonical units instead of embedding its own copy — the embedded
        # pre-#334 unit shadowed the packaged units in /etc/systemd/system
        # and drifted away from their hardening.
        self.assertNotIn("cat > /etc/systemd/system/chv-agent.service", installer)
        self.assertIn("packaging/systemd/chv-agent.service", installer)
        for directive in (
            "User=chv",
            "Group=chv",
            "UMask=002",
            "KillMode=process",
            "RuntimeDirectory=chv/agent",
            "RuntimeDirectoryMode=0775",
            "StateDirectory=chv/agent",
            "StateDirectoryMode=0700",
            "DeviceAllow=/dev/kvm rw",
            "DeviceAllow=/dev/net/tun rw",
        ):
            self.assertIn(directive, packaged)
            self.assertIn(directive, standalone)
        self.assertIn("ExecStartPre=+/usr/bin/install -d -m 0700", standalone)
        self.assertNotIn("ExecStartPost=", standalone)

    def test_agent_unit_allows_the_vm_nic_device(self):
        # Field blocker B1: any DeviceAllow switches the unit to a closed
        # device cgroup, and cloud-hypervisor runs as a child of this unit.
        # /dev/net/tun is world-writable but the device cgroup still returns
        # EPERM on open() unless the device is allow-listed, so every
        # virtio-net attach failed and the VM booted without a NIC.
        for relative in (
            "packaging/systemd/chv-agent.service",
            "docs/examples/systemd/chv-agent.service",
        ):
            unit = (ROOT / relative).read_text()
            self.assertIn("DeviceAllow=/dev/kvm rw", unit)
            self.assertIn("DeviceAllow=/dev/net/tun rw", unit)

    def test_install_surfaces_provision_exact_private_parents(self):
        postinstall = (ROOT / "packaging/scripts/postinstall.sh").read_text()
        tmpfiles = (ROOT / "packaging/tmpfiles/chv-node.conf").read_text()
        for path in ("/var/lib/chv/agent", "/run/chv/core"):
            self.assertIn(path, postinstall)
            self.assertRegex(tmpfiles, rf"d {re.escape(path)} 0700 chv chv -")
        self.assertRegex(tmpfiles, r"d /run/chv/agent 0775 chv chv -")

    def test_core_socket_and_runtime_lease_are_only_removed_by_explicit_wipe(self):
        # A normal stop/restart must never unlink Core's socket or its runtime
        # authority lease: doing so under a live Core splits authority. The one
        # explicit destructive path is install.sh --wipe, which regenerates the
        # node identity and therefore must remove the identity-bound Core
        # state (field blocker B5).
        forbidden = ("/run/chv/core/core-v1.sock", ".cellhv-runtime-authority.lease")
        installer = (ROOT / "scripts/install.sh").read_text()

        helper = re.search(
            r"remove_agent_authority_state\(\) \{(.*?)\n\}", installer, re.S
        )
        self.assertIsNotNone(helper, "the --wipe authority helper is missing")
        helper_body = helper.group(1)
        outside_helper = installer.replace(helper_body, "")

        for path in (ROOT / "packaging").rglob("*"):
            if not path.is_file():
                continue
            text = path.read_text(errors="ignore")
            for token in forbidden:
                for line in text.splitlines():
                    if token in line and re.search(r"\b(?:rm|remove|unlink)\b", line):
                        self.fail(f"{path}: destructive Core path line: {line}")
        for token in forbidden:
            for line in outside_helper.splitlines():
                if token in line and re.search(r"\b(?:rm|remove|unlink)\b", line):
                    self.fail(
                        "scripts/install.sh: destructive Core path line outside "
                        f"the --wipe helper: {line}"
                    )

        wipe_body = re.search(r"wipe_deployment\(\) \{(.*?)\n\}", installer, re.S).group(1)
        self.assertIn("remove_agent_authority_state", wipe_body)
        # Every Core writer must be stopped before its database/lease vanish.
        self.assertLess(
            wipe_body.index("remove_running_chv_processes"),
            wipe_body.index("remove_agent_authority_state"),
        )
        # The identity is regenerated only because /etc/chv is removed.
        self.assertLess(
            wipe_body.index("remove_agent_authority_state"),
            wipe_body.index("${CHV_CONFIG_DIR}"),
        )
        for expected in (
            "core.db",
            ".core.db.cellhv-runtime-authority.lease",
            "node-cache-v1.archive",
        ):
            self.assertIn(expected, helper_body)

    def test_default_seed_owns_the_gateway_once(self):
        # Field blocker B3: the legacy chvbr0 and nwd's bridge both carried the
        # seeded gateway, so the host routed guest traffic to chvbr0. When the
        # default network is seeded, the installer must leave the address to
        # chv-nwd and still provide forwarding/NAT, then verify single
        # ownership.
        installer = (ROOT / "scripts/install.sh").read_text()
        setup = re.search(r"setup_network\(\) \{(.*?)\n\}", installer, re.S).group(1)
        self.assertIn("enable_guest_forwarding", setup)
        # The seeded-network branch removes the legacy bridge (and its
        # persisted config/NAT) and returns before the legacy bridge is made.
        self.assertIn(
            'remove_chv_bridge_and_nat "${INSTALL_CHV_BRIDGE_NAME}"', setup
        )
        self.assertLess(
            setup.index('INSTALL_CHV_NO_SEED:-0'), setup.index("ip link add name")
        )
        self.assertLess(
            setup.index('INSTALL_CHV_NO_SEED:-0'),
            setup.index('ip addr add "${INSTALL_CHV_BRIDGE_CIDR}"'),
        )
        forwarding = re.search(
            r"enable_guest_forwarding\(\) \{(.*?)\n\}", installer, re.S
        ).group(1)
        self.assertIn("net.ipv4.ip_forward=1", forwarding)
        self.assertIn("MASQUERADE", forwarding)

        seed = re.search(r"seed_dev_resources\(\) \{(.*?)\n\}", installer, re.S).group(1)
        self.assertLess(
            seed.index("/v1/networks/create"),
            seed.index('assert_single_gateway_owner "${gateway_ip}"'),
        )
        owner = re.search(
            r"assert_single_gateway_owner\(\) \{(.*?)\n\}", installer, re.S
        ).group(1)
        self.assertIn("index($4, gw) == 1", owner)
        self.assertIn("sort -u", owner)
        self.assertIn("-ne 1", owner)
        self.assertIn("fatal", owner)

        # Exercise the match against real `ip -o -4 addr show` shape. `$4` is
        # `addr/prefix`, so an equality test against `10.200.0.1/` never
        # matches; the prefix guard must select only the exact gateway and
        # reject the look-alike 10.200.0.10.
        sample = (
            "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever\n"
            "2: chvbr0    inet 10.200.0.10/24 brd 10.200.0.255 scope global chvbr0\\       valid_lft forever\n"
            "3: br-eac31935dc4f    inet 10.200.0.1/24 brd 10.200.0.255 scope global br-eac31935dc4f\\       valid_lft forever\n"
        )
        result = subprocess.run(
            ["awk", "-v", "gw=10.200.0.1/", "index($4, gw) == 1 { print $2 }"],
            input=sample,
            text=True,
            capture_output=True,
            check=True,
        )
        self.assertEqual(result.stdout.split(), ["br-eac31935dc4f"])

    @unittest.skipUnless(shutil.which("bash"), "bash is required")
    def test_local_release_directory_reports_the_artifact_version(self):
        # Field evidence B7: the rc.6 clean-install transcript printed
        # "Version: 0.3.0" while installing 0.4.0-rc.6, because local
        # release-directory mode used the offline fallback pin. The completion
        # banner and the install record must name the artifact's own tag.
        source = (ROOT / "scripts/install.sh").read_text()
        helper = re.search(
            r"version_from_release_dir\(\) \{(.*?)\n\}", source, re.S
        ).group(0)
        self.assertIn("chv-*-linux-amd64", helper)
        self.assertIn('INSTALL_CHV_VERSION="$local_dir_version"', source)
        with tempfile.TemporaryDirectory() as tmp:
            cases = {
                "chv-0.4.0-rc.7-linux-amd64": "0.4.0-rc.7",
                "chv-0.4.0-rc.6-linux-amd64": "0.4.0-rc.6",
                "chv-0.3.0-linux-amd64": "0.3.0",
                "chv-0.4.0-rc.7-linux-amd64.tar.gz": "0.4.0-rc.7",
                "not-a-release": "",
                "chv-0.4.0-linux-amd64.bak": "",
            }
            script = "set -euo pipefail\n" + helper + "\n"
            for name in cases:
                path = pathlib.Path(tmp) / name
                if name.endswith(".tar.gz"):
                    path.write_text("")
                else:
                    path.mkdir()
                script += (
                    f"printf '%s\\n' \"$(version_from_release_dir '{path}')\"\n"
                )
            out = subprocess.run(
                ["bash", "-c", script], check=True, capture_output=True, text=True
            ).stdout.splitlines()
        self.assertEqual(out, list(cases.values()))

    @unittest.skipUnless(shutil.which("bash"), "bash is required")
    def test_wipe_removes_identity_bound_core_state_but_keeps_other_agent_state(self):
        # Exercise the helper itself: only the identity-bound authority files
        # may disappear; certificates and anything else in the agent dir stay.
        source = (ROOT / "scripts/install.sh").read_text()
        helper = re.search(
            r"remove_agent_authority_state\(\) \{(.*?)\n\}", source, re.S
        ).group(0)
        with tempfile.TemporaryDirectory() as tmp:
            data = pathlib.Path(tmp) / "chv"
            agent = data / "agent"
            agent.mkdir(parents=True)
            for name in (
                "core.db",
                "core.db-wal",
                "core.db-shm",
                "core.db-journal",
                ".core.db.cellhv-runtime-authority.lease",
                "node-cache-v1.archive",
                ".node-cache-v1.archive.cellhv-authority.lock",
            ):
                (agent / name).write_text("state")
            keep = agent / "agent.crt"
            keep.write_text("keep")
            script = (
                "set -euo pipefail\n"
                f"CHV_DATA_DIR='{data}'\n"
                f"{helper}\n"
                "remove_agent_authority_state\n"
            )
            subprocess.run(
                ["bash", "-c", script], check=True, capture_output=True, text=True
            )
            remaining = sorted(p.name for p in agent.iterdir())
        self.assertEqual(remaining, ["agent.crt"])

    def test_removal_paths_terminate_vms_left_running_by_the_agent(self):
        # `KillMode=process` lets VMMs outlive chv-agent by design, so every
        # final removal path (install.sh wipe/uninstall, quick-install
        # --uninstall, package remove/purge) must stop the survivors itself.
        # Otherwise removal leaves orphaned guests with their taps and
        # nftables state behind.
        installer = (ROOT / "scripts/install.sh").read_text()
        quick = (ROOT / "scripts/quick-install.sh").read_text()
        preremove = (ROOT / "packaging/scripts/preremove.sh").read_text()

        self.assertIn("list_chv_vmm_processes()", installer)
        runtime_body = re.search(
            r"list_chv_runtime_processes\(\) \{(.*?)\n\}", installer, re.S
        )
        self.assertIsNotNone(runtime_body)
        self.assertIn("list_chv_vmm_processes", runtime_body.group(1))

        wipe_body = re.search(r"wipe_deployment\(\) \{(.*?)\n\}", installer, re.S)
        self.assertIsNotNone(wipe_body)
        wipe_body = wipe_body.group(1)
        self.assertIn("remove_running_chv_vmms", wipe_body)
        # The agent must be down before its VMMs are signalled, or it would
        # reconcile them straight back; storage/network stop last.
        self.assertLess(wipe_body.index("chv-agent"), wipe_body.index("remove_running_chv_vmms"))
        self.assertLess(wipe_body.index("remove_running_chv_vmms"), wipe_body.index("chv-stord"))

        clean_body = re.search(
            r"clean_previous_install\(\) \{(.*?)\n\}", installer, re.S
        ).group(1)
        self.assertIn("remove_running_chv_vmms", clean_body)
        self.assertLess(clean_body.index("chv-agent"), clean_body.index("remove_running_chv_vmms"))
        self.assertLess(clean_body.index("remove_running_chv_vmms"), clean_body.index("chv-stord"))

        uninstall_body = re.search(r"^uninstall\(\) \{(.*?)^\}", quick, re.S | re.M)
        self.assertIsNotNone(uninstall_body)
        uninstall_body = uninstall_body.group(1)
        self.assertIn("terminate_chv_vmms", uninstall_body)
        self.assertLess(uninstall_body.index("chv-agent"), uninstall_body.index("terminate_chv_vmms"))
        self.assertLess(uninstall_body.index("terminate_chv_vmms"), uninstall_body.index("chv-stord"))

        # Package removal is final, but an upgrade must leave running VMs
        # alone for the restarted agent to adopt.
        self.assertIn('if [ "$ACTION" = "remove" ]', preremove)
        remove_branch = preremove.split('if [ "$ACTION" = "remove" ]', 1)[1]
        self.assertIn("kill -TERM $vmm_pids", remove_branch)
        self.assertIn("kill -KILL $vmm_pids", remove_branch)

        # A timed-out/SIGKILLed stop leaves the unit in systemd's in-memory
        # "failed" set even after its file is gone; complete removal clears it.
        for body in (wipe_body, clean_body, uninstall_body):
            self.assertIn("reset-failed", body)
        self.assertIn("reset-failed", remove_branch)

    def test_removal_paths_tear_down_chv_owned_host_network_state(self):
        # nwd's bridges, netns, taps and per-network nftables tables are host
        # state outside /var/lib/chv. Package removal, quick-install
        # --uninstall, and install.sh wipe must remove them themselves:
        # "network residue that survives documented cleanup" is blocker-class
        # in the field plan (§6.9).
        installer = (ROOT / "scripts/install.sh").read_text()
        quick = (ROOT / "scripts/quick-install.sh").read_text()
        preremove = (ROOT / "packaging/scripts/preremove.sh").read_text()
        for relative, text in (
            ("scripts/install.sh", installer),
            ("scripts/quick-install.sh", quick),
            ("packaging/scripts/preremove.sh", preremove),
        ):
            self.assertIn("delete_chv_network_state()", text, relative)
            self.assertIn("remove_chv_bridge_and_nat()", text, relative)
            self.assertIn("chv_bridge_name_for_network()", text, relative)
            # Only the CHV-owned `chv-` nft namespace may be swept; `chvhs-*`
            # host-safety test tables are not ours to delete.
            self.assertIn("table inet chv-", text, relative)
            # The removal copies must not drift: each tears down the live
            # MASQUERADE rule and the persisted config that recreates it.
            self.assertIn("iptables -t nat -D POSTROUTING", text, relative)
            self.assertIn("sysctl.d/99-chv-forward.conf", text, relative)
            self.assertIn('CHV_IPTABLES_RULES_V4:-${CHV_HOST_ETC}/iptables/rules.v4', text, relative)

        clean_body = re.search(
            r"clean_previous_install\(\) \{(.*?)\n\}", installer, re.S
        ).group(1)
        self.assertIn("delete_chv_network_state", clean_body)
        wipe_body = re.search(r"wipe_deployment\(\) \{(.*?)\n\}", installer, re.S).group(1)
        self.assertIn("delete_chv_network_state", wipe_body)
        # The database is the most complete source of network ids; tear down
        # before the wipe deletes it.
        self.assertLess(
            wipe_body.index("delete_chv_network_state"), wipe_body.index("controlplane.db")
        )

        uninstall_body = re.search(r"^uninstall\(\) \{(.*?)^\}", quick, re.S | re.M).group(1)
        # nwd must be down before its bridges/netns are deleted, and the
        # teardown must precede both the data purge and the package removal
        # that deletes the database.
        self.assertLess(uninstall_body.index("chv-nwd"), uninstall_body.index("delete_chv_network_state"))
        self.assertLess(uninstall_body.index("delete_chv_network_state"), uninstall_body.index("ARG_PURGE"))
        # The install.sh (qualified) layout owns the UI tree and nginx site;
        # a complete uninstall must not leave a stale enabled site behind.
        self.assertIn("/opt/chv", uninstall_body)
        self.assertIn("/etc/nginx/sites-enabled/chv", uninstall_body)

        before_remove, remove_branch = preremove.split('if [ "$ACTION" = "remove" ]', 1)
        self.assertNotIn("\n    delete_chv_network_state\n", before_remove)
        self.assertIn("\n    delete_chv_network_state\n", remove_branch)

    @unittest.skipUnless(shutil.which("bash") and shutil.which("sh"), "shells required")
    def test_bridge_name_derivation_shell_vectors(self):
        # The shells reimplement chv_common::bridge_name_for_network (the
        # kernel caps device names at 15 chars; long ids keep an 8-char prefix
        # plus a 4-hex FNV-1a suffix). The Rust side owns the same vectors in
        # crates/chv-common/src/lib.rs; these assertions pin the sh copies.
        # A drift here deletes nothing or the wrong device.
        vectors = {
            "default": "chvbr0",
            "0b7f28f0": "br-0b7f28f0",
            "63b27631c9c92fda4a43351c5aeeba21": "br-63b276315a70",
        }
        for relative, shell in (
            ("scripts/install.sh", "bash"),
            ("scripts/quick-install.sh", "bash"),
            ("packaging/scripts/preremove.sh", "sh"),
        ):
            text = (ROOT / relative).read_text()
            match = re.search(r"chv_bridge_name_for_network\(\) \{(.*?)\n\}", text, re.S)
            self.assertIsNotNone(match, relative)
            function = "chv_bridge_name_for_network() {" + match.group(1) + "\n}"
            for network_id, expected in vectors.items():
                result = subprocess.run(
                    [shell, "-c", function + '\nchv_bridge_name_for_network "$1"', "_", network_id],
                    capture_output=True,
                    text=True,
                    check=True,
                )
                self.assertEqual(result.stdout.strip(), expected, f"{relative}: {network_id}")

    @unittest.skipUnless(shutil.which("bash"), "bash is required to exercise the cleanup")
    def test_network_cleanup_deletes_only_chv_owned_state(self):
        # Exercise every shipped copy of the removal teardown. install.sh and
        # quick-install.sh are bash; preremove.sh must also run under dash,
        # which is what dpkg invokes. Each case gets a fresh sandbox so the
        # fakes cannot leak state between runs.
        bash = shutil.which("bash")
        cases = [
            ("scripts/install.sh", bash),
            ("scripts/quick-install.sh", bash),
        ]
        posix_shell = shutil.which("dash") or shutil.which("sh")
        if posix_shell:
            cases.append(("packaging/scripts/preremove.sh", posix_shell))
        # preremove.sh ships as POSIX sh, but running the same functions
        # under bash+pipefail pins the `|| true` guards that keep a failing
        # nft/ip/route lookup from aborting the teardown.
        if bash:
            cases.append(("packaging/scripts/preremove.sh", bash))
        for relative, shell in cases:
            with self.subTest(script=relative, shell=os.path.basename(shell)):
                self._exercise_network_cleanup(relative, shell)

    def _exercise_network_cleanup(self, relative, shell):
        # Run the shipped functions against fake ip/nft/sqlite3/iptables and a
        # redirected CHV_HOST_ETC, so the test can never touch the real host's
        # bridges, namespaces, firewall, or networkd config.
        source = (ROOT / relative).read_text()
        functions = "\n".join(
            re.search(rf"^{name}\(\) \{{.*?\n\}}", source, re.S | re.M).group(0)
            for name in (
                "chv_bridge_name_for_network",
                "chv_network_ids",
                "chv_network_cidr",
                "remove_chv_bridge_and_nat",
                "delete_chv_network_state",
            )
        )

        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            bin_dir = tmp_path / "bin"
            bin_dir.mkdir()
            log = tmp_path / "calls.log"
            log.write_text("")
            iptables_state = tmp_path / "iptables.state"
            # Both CHV NAT forms, live in the kernel table, for the baseline
            # bridge, the two hex-id networks, and an operator-chosen id.
            iptables_state.write_text(
                "POSTROUTING -s 10.200.0.0/24 ! -d 10.200.0.0/24 -j MASQUERADE\n"
                "POSTROUTING -s 10.200.0.0/24 ! -o chvbr0 -j MASQUERADE\n"
                "POSTROUTING -s 10.201.0.0/24 ! -d 10.201.0.0/24 -j MASQUERADE\n"
                "POSTROUTING -s 10.202.0.0/24 ! -d 10.202.0.0/24 -j MASQUERADE\n"
                "POSTROUTING -s 10.203.0.0/24 ! -d 10.203.0.0/24 -j MASQUERADE\n"
                # `br-gone` is absent from the live links below, so only the
                # NAT rules that still name it can identify the CIDR.
                "POSTROUTING -s 10.204.0.0/24 ! -d 10.204.0.0/24 -j MASQUERADE\n"
                "POSTROUTING -s 10.204.0.0/24 ! -o br-gone -j MASQUERADE\n"
                # `br-multi` carries two historical `! -o` rules; both CIDRs
                # must be recovered, not just the first.
                "POSTROUTING -s 10.207.0.0/24 ! -d 10.207.0.0/24 -j MASQUERADE\n"
                "POSTROUTING -s 10.207.0.0/24 ! -o br-multi -j MASQUERADE\n"
                "POSTROUTING -s 10.208.0.0/24 ! -d 10.208.0.0/24 -j MASQUERADE\n"
                "POSTROUTING -s 10.208.0.0/24 ! -o br-multi -j MASQUERADE\n"
                # `br-mix` has a live route (10.209) plus a stale historical
                # `! -o` rule for 10.210: both must be cleaned, so the sources
                # are unioned rather than tried in priority order.
                "POSTROUTING -s 10.209.0.0/24 ! -d 10.209.0.0/24 -j MASQUERADE\n"
                "POSTROUTING -s 10.210.0.0/24 ! -o br-mix -j MASQUERADE\n"
            )

            # A persisted-rules file with CHV lines and one that belongs to
            # something else: only the former may be removed.
            rules = tmp_path / "rules.v4"
            rules.write_text(
                "*nat\n"
                "-A POSTROUTING -s 10.200.0.0/24 ! -d 10.200.0.0/24 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.200.0.0/24 ! -o chvbr0 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.201.0.0/24 ! -d 10.201.0.0/24 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.203.0.0/24 ! -d 10.203.0.0/24 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.204.0.0/24 ! -d 10.204.0.0/24 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.204.0.0/24 ! -o br-gone -j MASQUERADE\n"
                "-A POSTROUTING -s 10.207.0.0/24 ! -d 10.207.0.0/24 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.207.0.0/24 ! -o br-multi -j MASQUERADE\n"
                "-A POSTROUTING -s 10.208.0.0/24 ! -d 10.208.0.0/24 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.208.0.0/24 ! -o br-multi -j MASQUERADE\n"
                "-A POSTROUTING -s 10.209.0.0/24 ! -d 10.209.0.0/24 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.210.0.0/24 ! -o br-mix -j MASQUERADE\n"
                "-A POSTROUTING -s 10.9.9.0/24 -j MASQUERADE\n"
                "COMMIT\n"
            )

            etc = tmp_path / "etc"
            networkd = etc / "systemd/network"
            networkd.mkdir(parents=True)
            (networkd / "10-chvbr0.netdev").write_text("[NetDev]\nName=chvbr0\n")
            (networkd / "11-chvbr0.network").write_text("[Match]\nName=chvbr0\n")
            (networkd / "12-ens19.network").write_text("[Network]\nBridge=chvbr0\n")
            (networkd / "12-eth9.network").write_text("[Network]\nBridge=otherbr\n")
            ifupdown = etc / "network/interfaces.d"
            ifupdown.mkdir(parents=True)
            (ifupdown / "chvbr0").write_text("auto chvbr0\n")
            sysctl = etc / "sysctl.d"
            sysctl.mkdir(parents=True)
            (sysctl / "99-chv-forward.conf").write_text("net.ipv4.ip_forward=1\n")

            links = tmp_path / "links"
            links.write_text(
                "tap-1234abcd\ntap-deadbeef\ntap-cafebabe\n"
                "br-63b276315a70\nbr-0b7f28f0\nbr-net1\nbr-multi\nbr-mix\n"
                "chvbr0\nens19\n"
            )
            ip = bin_dir / "ip"
            ip.write_text(
                "#!/bin/bash\n"
                f"printf 'ip %s\\n' \"$*\" >> {log}\n"
                f"links={links}\n"
                'args="$*"\n'
                'case "$args" in\n'
                '  "netns list") printf \'%s\\n\' ns-63b27631c9c92fda4a43351c5aeeba21 '
                "ns-default ns-net1 ns-gone ns-other ;;\n"
                '  "-o -4 route show dev "*" proto kernel scope link")\n'
                '    dev="${args#-o -4 route show dev }"\n'
                '    dev="${dev% proto kernel scope link}"\n'
                '    grep -Fxq "$dev" "$links" || exit 1\n'
                '    case "$dev" in\n'
                "      br-63b276315a70) printf '%s\\n' '10.201.0.0/24 proto kernel scope link src 10.201.0.1' ;;\n"
                "      br-0b7f28f0) printf '%s\\n' '10.202.0.0/24 proto kernel scope link src 10.202.0.1' ;;\n"
                "      br-net1) printf '%s\\n' '10.203.0.0/24 proto kernel scope link src 10.203.0.1' ;;\n"
                "      br-mix) printf '%s\\n' '10.209.0.0/24 proto kernel scope link src 10.209.0.1' ;;\n"
                "      chvbr0) printf '%s\\n' '10.200.0.0/24 proto kernel scope link src 10.200.0.1' ;;\n"
                '      *) exit 1 ;;\n'
                '    esac ;;\n'
                '  "-o link show master "*)\n'
                '    dev="${args#-o link show master }"\n'
                '    grep -Fxq "$dev" "$links" || exit 1\n'
                '    case "$dev" in\n'
                "      br-63b276315a70) printf '%s\\n' '9: tap-1234abcd@if5: <BROADCAST> mtu 1500 master br-63b276315a70' "
                "'10: ens19: <BROADCAST> mtu 1500 master br-63b276315a70' ;;\n"
                "      br-0b7f28f0) printf '%s\\n' '11: tap-deadbeef@if7: <BROADCAST> mtu 1500 master br-0b7f28f0' ;;\n"
                "      br-net1) printf '%s\\n' '12: tap-cafebabe@if9: <BROADCAST> mtu 1500 master br-net1' ;;\n"
                '      *) : ;;\n'
                '    esac ;;\n'
                '  "link del dev "*)\n'
                '    dev="${args#link del dev }"\n'
                '    grep -Fxv "$dev" "$links" > "$links.tmp" || true\n'
                '    mv "$links.tmp" "$links" ;;\n'
                '  *) : ;;\n'
                "esac\n"
            )
            nft = bin_dir / "nft"
            nft.write_text(
                "#!/bin/bash\n"
                f"printf 'nft %s\\n' \"$*\" >> {log}\n"
                'if [ "$1 $2" = "list tables" ]; then\n'
                "  printf '%s\\n' 'table inet chv-63b27631c9c92fda4a43351c5aeeba21' "
                "'table inet chv-0b7f28f0' 'table inet chv-default' 'table inet chv-net1' "
                "'table inet chv-gone' 'table inet chv-multi' 'table inet chv-mix' "
                "'table inet chvhs-deadbeef' 'table ip filter'\n"
                "fi\n"
            )
            sqlite3 = bin_dir / "sqlite3"
            sqlite3.write_text(
                "#!/bin/bash\n" f"printf 'sqlite3 %s\\n' \"$*\" >> {log}\n"
            )
            iptables = bin_dir / "iptables"
            iptables.write_text(
                "#!/bin/bash\n"
                f"printf 'iptables %s\\n' \"$*\" >> {log}\n"
                'action="$3"\n'
                "shift 3\n"
                'rule="$*"\n'
                'case "$action" in\n'
                f'  -C) grep -Fxq "$rule" {iptables_state} ;;\n'
                f"  -A) grep -Fxq \"$rule\" {iptables_state} || printf '%s\\\\n' \"$rule\" >> {iptables_state} ;;\n"
                "  -D)\n"
                f'    grep -Fxq "$rule" {iptables_state} || exit 1\n'
                f'    grep -Fxv "$rule" {iptables_state} > {iptables_state}.tmp || true\n'
                f'    mv {iptables_state}.tmp {iptables_state}\n'
                "    ;;\n"
                f"  -S) sed 's/^/-A /' {iptables_state} ;;\n"
                "esac\n"
            )
            for tool in (ip, nft, sqlite3, iptables):
                tool.chmod(0o755)

            # Never let the teardown call the host's real
            # netfilter-persistent, which would rewrite the host's persisted
            # iptables rules.
            netfilter_persistent = bin_dir / "netfilter-persistent"
            netfilter_persistent.write_text(
                "#!/bin/bash\n"
                f"printf 'netfilter-persistent %s\\n' \"$*\" >> {log}\n"
            )
            netfilter_persistent.chmod(0o755)

            pipefail = "-o pipefail" if "bash" in (shell or "") else ""
            script = (
                f"set -eu {pipefail}\n".replace("  ", " ")
                + 'cmd_exists() { command -v "$1" >/dev/null 2>&1; }\n'
                'have() { command -v "$1" >/dev/null 2>&1; }\n'
                "CHV_DB_PATH=/nonexistent.db\n"
                "CHV_USER=chv-no-such-user\n"
                "INSTALL_CHV_BRIDGE_NAME=chvbr0\n"
                "INSTALL_CHV_BRIDGE_CIDR=10.200.0.1/24\n"
                f"CHV_HOST_ETC='{etc}'\n"
                f"CHV_IPTABLES_RULES_V4='{rules}'\n"
                f"{functions}\n"
                "delete_chv_network_state\n"
            )
            env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}")
            subprocess.run(
                [shell, "-c", script],
                env=env,
                check=True,
                capture_output=True,
                text=True,
            )
            calls = log.read_text()
            persisted = rules.read_text()
            remaining = sorted(p.name for p in networkd.iterdir())
            ifupdown_remaining = sorted(p.name for p in ifupdown.iterdir())
            sysctl_remaining = sorted(p.name for p in sysctl.iterdir())

        for expected in (
            "ip link del dev tap-1234abcd",
            "ip link del dev tap-deadbeef",
            "ip link del dev tap-cafebabe",
            "ip link del dev br-63b276315a70",
            "ip link del dev br-0b7f28f0",
            "ip link del dev br-net1",
            "ip netns del ns-63b27631c9c92fda4a43351c5aeeba21",
            "ip netns del ns-default",
            "ip netns del ns-net1",
            "ip netns del ns-gone",
            "ip netns del ns-multi",
            "ip netns del ns-mix",
            "nft delete table inet chv-63b27631c9c92fda4a43351c5aeeba21",
            "nft delete table inet chv-0b7f28f0",
            "nft delete table inet chv-default",
            "nft delete table inet chv-net1",
            "nft delete table inet chv-gone",
            "nft delete table inet chv-multi",
            "nft delete table inet chv-mix",
            "ip link del dev chvbr0",
            # Live NAT rules are deleted in both historical forms.
            "iptables -t nat -D POSTROUTING -s 10.200.0.0/24 ! -d 10.200.0.0/24 -j MASQUERADE",
            "iptables -t nat -D POSTROUTING -s 10.200.0.0/24 ! -o chvbr0 -j MASQUERADE",
            "iptables -t nat -D POSTROUTING -s 10.201.0.0/24 ! -d 10.201.0.0/24 -j MASQUERADE",
            "iptables -t nat -D POSTROUTING -s 10.203.0.0/24 ! -d 10.203.0.0/24 -j MASQUERADE",
            # `br-gone` has no live route: the CIDR is recovered from the
            # surviving rule that still names the bridge.
            "iptables -t nat -D POSTROUTING -s 10.204.0.0/24 ! -d 10.204.0.0/24 -j MASQUERADE",
            "iptables -t nat -D POSTROUTING -s 10.204.0.0/24 ! -o br-gone -j MASQUERADE",
            # Two CIDRs survive for one bridge: both are recovered.
            "iptables -t nat -D POSTROUTING -s 10.207.0.0/24 ! -d 10.207.0.0/24 -j MASQUERADE",
            "iptables -t nat -D POSTROUTING -s 10.207.0.0/24 ! -o br-multi -j MASQUERADE",
            "iptables -t nat -D POSTROUTING -s 10.208.0.0/24 ! -d 10.208.0.0/24 -j MASQUERADE",
            "iptables -t nat -D POSTROUTING -s 10.208.0.0/24 ! -o br-multi -j MASQUERADE",
            # A live route and a stale `! -o` rule for the same bridge: both
            # CIDRs are cleaned.
            "iptables -t nat -D POSTROUTING -s 10.209.0.0/24 ! -d 10.209.0.0/24 -j MASQUERADE",
            "iptables -t nat -D POSTROUTING -s 10.210.0.0/24 ! -o br-mix -j MASQUERADE",
            # The persisted rules are re-saved through the fake, never the
            # host's real netfilter-persistent.
            "netfilter-persistent save",
        ):
            self.assertIn(expected, calls, relative)
        for forbidden in (
            "ip link del dev ens19",
            "ns-other",
            "chvhs-deadbeef",
            "iptables -t nat -D POSTROUTING -s 10.9.9.0/24",
        ):
            self.assertNotIn(forbidden, calls, relative)

        # Only the CHV rules disappear from the persisted file.
        self.assertNotIn("10.200.0.0/24", persisted, relative)
        self.assertNotIn("10.201.0.0/24", persisted, relative)
        self.assertNotIn("10.203.0.0/24", persisted, relative)
        self.assertNotIn("10.204.0.0/24", persisted, relative)
        self.assertNotIn("10.207.0.0/24", persisted, relative)
        self.assertNotIn("10.208.0.0/24", persisted, relative)
        self.assertNotIn("10.209.0.0/24", persisted, relative)
        self.assertNotIn("10.210.0.0/24", persisted, relative)
        self.assertIn("-A POSTROUTING -s 10.9.9.0/24 -j MASQUERADE", persisted, relative)

        # Bridging config that would recreate the CHV bridge is gone; another
        # bridge's drop-in and the uplink's non-CHV config are untouched.
        self.assertEqual(remaining, ["12-eth9.network"], relative)
        self.assertEqual(ifupdown_remaining, [], relative)
        self.assertEqual(sysctl_remaining, [], relative)

    @unittest.skipUnless(
        shutil.which("bash") and shutil.which("sh"), "shells required"
    )
    def test_network_cleanup_uses_database_ids_and_upgrade_is_a_noop(self):
        # The database is the only source that knows about a network whose
        # nft table and namespace are already gone. Exercise it for every
        # shipped copy, then prove the package upgrade path touches nothing.
        cases = [
            ("scripts/install.sh", shutil.which("bash")),
            ("scripts/quick-install.sh", shutil.which("bash")),
        ]
        posix_shell = shutil.which("dash") or shutil.which("sh")
        if posix_shell:
            cases.append(("packaging/scripts/preremove.sh", posix_shell))
        for relative, shell in cases:
            with self.subTest(script=relative):
                self._exercise_database_id_source(relative, shell)
        self._assert_install_baseline_cidr_fallback()
        self._assert_preremove_upgrade_is_a_noop()

    def _exercise_database_id_source(self, relative, shell):
        source = (ROOT / relative).read_text()
        functions = "\n".join(
            re.search(rf"^{name}\(\) \{{.*?\n\}}", source, re.S | re.M).group(0)
            for name in (
                "chv_bridge_name_for_network",
                "chv_network_ids",
                "chv_network_cidr",
                "remove_chv_bridge_and_nat",
                "delete_chv_network_state",
            )
        )

        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            bin_dir = tmp_path / "bin"
            bin_dir.mkdir()
            log = tmp_path / "calls.log"
            log.write_text("")
            db = tmp_path / "controlplane.db"
            db.write_text("")  # existence is what chv_network_ids checks
            rules = tmp_path / "rules.v4"
            # Only the current `! -d` form survives for `gonecur`, so no rule
            # names its bridge and the route lookup fails: the database hint
            # is the only remaining source of the CIDR.
            rules.write_text(
                "*nat\n"
                "-A POSTROUTING -s 10.205.0.0/24 ! -d 10.205.0.0/24 -j MASQUERADE\n"
                "-A POSTROUTING -s 10.9.9.0/24 -j MASQUERADE\n"
                "COMMIT\n"
            )
            iptables_state = tmp_path / "iptables.state"
            iptables_state.write_text(
                "POSTROUTING -s 10.205.0.0/24 ! -d 10.205.0.0/24 -j MASQUERADE\n"
            )
            etc = tmp_path / "etc"
            (etc / "sysctl.d").mkdir(parents=True)
            (etc / "sysctl.d/99-chv-forward.conf").write_text(
                "net.ipv4.ip_forward=1\n"
            )

            # No nft tables and no namespaces are reported, so `dbonly` and
            # `gonecur` can only come from the database. Every link lookup
            # fails, like iproute2 on a removed bridge.
            ip = bin_dir / "ip"
            ip.write_text(
                "#!/bin/bash\n"
                f"printf 'ip %s\\n' \"$*\" >> {log}\n"
                'case "$*" in\n'
                "  'netns list') printf '%s\\n' ns-other ;;\n"
                "  *) exit 1 ;;\n"
                "esac\n"
            )
            nft = bin_dir / "nft"
            nft.write_text(
                "#!/bin/bash\n"
                f"printf 'nft %s\\n' \"$*\" >> {log}\n"
                'if [ "$1 $2" = "list tables" ]; then\n'
                "  printf '%s\\n' 'table inet filter'\n"
                "fi\n"
            )
            sqlite3 = bin_dir / "sqlite3"
            sqlite3.write_text(
                "#!/bin/bash\n"
                f"printf 'sqlite3 %s\\n' \"$*\" >> {log}\n"
                'query="$*"\n'
                'case "$query" in\n'
                "  *\"SELECT cidr FROM network_desired_state WHERE network_id = 'gonecur'\"*)\n"
                "    printf '%s\\n' 10.205.0.0/24 ;;\n"
                "  *\"SELECT cidr FROM network_desired_state WHERE network_id = 'default'\"*)\n"
                "    printf '%s\\n' 10.200.0.0/24 ;;\n"
                "  *\"SELECT cidr FROM network_desired_state WHERE network_id = 'badcidr'\"*)\n"
                "    printf '%s\\n' '10.211.0.0/24; touch /tmp/chv-pwned' ;;\n"
                '  *"SELECT network_id FROM network_desired_state"*)\n'
                "    printf '%s\\n' dbonly gonecur badcidr ;;\n"
                "  *) : ;;\n"
                "esac\n"
            )
            iptables = bin_dir / "iptables"
            iptables.write_text(
                "#!/bin/bash\n"
                f"printf 'iptables %s\\n' \"$*\" >> {log}\n"
                'action="$3"\n'
                "shift 3\n"
                'rule="$*"\n'
                'case "$action" in\n'
                f'  -C) grep -Fxq "$rule" {iptables_state} ;;\n'
                f"  -A) grep -Fxq \"$rule\" {iptables_state} || printf '%s\\\\n' \"$rule\" >> {iptables_state} ;;\n"
                "  -D)\n"
                f'    grep -Fxq "$rule" {iptables_state} || exit 1\n'
                f'    grep -Fxv "$rule" {iptables_state} > {iptables_state}.tmp || true\n'
                f'    mv {iptables_state}.tmp {iptables_state}\n'
                "    ;;\n"
                f"  -S) sed 's/^/-A /' {iptables_state} ;;\n"
                "esac\n"
            )
            netfilter_persistent = bin_dir / "netfilter-persistent"
            netfilter_persistent.write_text(
                "#!/bin/bash\n"
                f"printf 'netfilter-persistent %s\\n' \"$*\" >> {log}\n"
            )
            chown = bin_dir / "chown"
            chown.write_text(
                "#!/bin/bash\n" f"printf 'chown %s\\n' \"$*\" >> {log}\n"
            )
            for tool in (
                ip,
                nft,
                sqlite3,
                iptables,
                netfilter_persistent,
                chown,
            ):
                tool.chmod(0o755)

            pipefail = "-o pipefail" if "bash" in (shell or "") else ""
            script = (
                f"set -eu {pipefail}\n".replace("  ", " ")
                + 'cmd_exists() { command -v "$1" >/dev/null 2>&1; }\n'
                'have() { command -v "$1" >/dev/null 2>&1; }\n'
                # Passthroughs force the plain-sqlite3 path deterministically.
                # The callers use a user that exists on no host, and
                # preremove's real helper would otherwise take the runuser
                # branch on hosts that have a chv user.
                'chv_sqlite() { sqlite3 "$@"; }\n'
                'chv_sqlite_read() { sqlite3 "$@"; }\n'
                f"CHV_DB_PATH='{db}'\n"
                "CHV_USER=chv-no-such-user\n"
                "INSTALL_CHV_BRIDGE_NAME=chvbr0\n"
                "INSTALL_CHV_BRIDGE_CIDR=10.200.0.1/24\n"
                f"CHV_HOST_ETC='{etc}'\n"
                f"CHV_IPTABLES_RULES_V4='{rules}'\n"
                f"{functions}\n"
                "delete_chv_network_state\n"
            )
            subprocess.run(
                [shell, "-c", script],
                env=dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}"),
                check=True,
                capture_output=True,
                text=True,
            )
            calls = log.read_text()
            persisted = rules.read_text()

        self.assertIn("sqlite3", calls, relative)
        self.assertIn("ip netns del ns-dbonly", calls, relative)
        self.assertIn("nft delete table inet chv-dbonly", calls, relative)
        self.assertIn("ip link del dev br-dbonly", calls, relative)
        # `gonecur` has no route and no rule naming its bridge; only the
        # database row can supply the CIDR for the current NAT form.
        self.assertIn("ip netns del ns-gonecur", calls, relative)
        self.assertIn("nft delete table inet chv-gonecur", calls, relative)
        self.assertIn("ip link del dev br-gonecur", calls, relative)
        self.assertIn(
            "iptables -t nat -D POSTROUTING -s 10.205.0.0/24 "
            "! -d 10.205.0.0/24 -j MASQUERADE",
            calls,
            relative,
        )
        # A free-text database cidr is not trusted: the id is still torn
        # down, but the invalid hint never reaches iptables.
        self.assertIn("ip link del dev br-badcidr", calls, relative)
        self.assertIn("nft delete table inet chv-badcidr", calls, relative)
        self.assertNotIn("10.211.0.0/24", calls, relative)
        self.assertFalse(
            pathlib.Path("/tmp/chv-pwned").exists(),
            "a cidr hint must never execute as shell",
        )
        self.assertNotIn("10.205.0.0/24", persisted, relative)
        self.assertIn(
            "-A POSTROUTING -s 10.9.9.0/24 -j MASQUERADE", persisted, relative
        )
        # The baseline bridge has no route either, so the `default` row's
        # CIDR drives the attempted current-form deletion.
        self.assertIn(
            "iptables -t nat -C POSTROUTING -s 10.200.0.0/24 "
            "! -d 10.200.0.0/24 -j MASQUERADE",
            calls,
            relative,
        )
        self.assertIn("netfilter-persistent save", calls, relative)
        # A root sqlite3 open can create root-owned WAL sidecars. Both the
        # id lookup and the new cidr lookup hand them back to the service
        # user (the id lookup runs before any SELECT cidr). The bash copies
        # derive the owner from $CHV_USER; preremove.sh hardcodes `chv`.
        owner = (
            "chv:chv"
            if relative == "packaging/scripts/preremove.sh"
            else "chv-no-such-user:chv-no-such-user"
        )
        self.assertGreaterEqual(calls.count(f"chown {owner}"), 2, relative)
        before_cidr, after_cidr = calls.split("SELECT cidr", 1)
        self.assertIn(f"chown {owner}", before_cidr, relative)
        self.assertIn(f"chown {owner}", after_cidr, relative)

    def _assert_install_baseline_cidr_fallback(self):
        # install.sh writes the baseline NAT rule from INSTALL_CHV_BRIDGE_CIDR.
        # If the database has no `default` row and the bridge is already gone,
        # that value is the only source of the subnet. Cover a /24 and a
        # host-bit-carrying /16, which install.sh writes verbatim.
        source = (ROOT / "scripts/install.sh").read_text()
        functions = "\n".join(
            re.search(rf"^{name}\(\) \{{.*?\n\}}", source, re.S | re.M).group(0)
            for name in (
                "chv_bridge_name_for_network",
                "chv_network_ids",
                "chv_network_cidr",
                "remove_chv_bridge_and_nat",
                "delete_chv_network_state",
            )
        )
        for configured, expected in (
            ("10.206.0.1/24", "10.206.0.0/24"),
            ("10.206.5.1/16", "10.206.5.0/16"),
        ):
            with self.subTest(configured=configured):
                with tempfile.TemporaryDirectory() as tmp:
                    tmp_path = pathlib.Path(tmp)
                    bin_dir = tmp_path / "bin"
                    bin_dir.mkdir()
                    log = tmp_path / "calls.log"
                    log.write_text("")
                    etc = tmp_path / "etc"
                    (etc / "sysctl.d").mkdir(parents=True)
                    rules = tmp_path / "rules.v4"
                    rules.write_text("*nat\nCOMMIT\n")
                    ip = bin_dir / "ip"
                    ip.write_text(
                        "#!/bin/bash\n"
                        f"printf 'ip %s\\n' \"$*\" >> {log}\n"
                        "exit 1\n"
                    )
                    nft = bin_dir / "nft"
                    nft.write_text(
                        "#!/bin/bash\n"
                        f"printf 'nft %s\\n' \"$*\" >> {log}\n"
                    )
                    iptables = bin_dir / "iptables"
                    iptables.write_text(
                        "#!/bin/bash\n"
                        f"printf 'iptables %s\\n' \"$*\" >> {log}\n"
                        'case "$3" in\n'
                        "  -C) exit 1 ;;\n"
                        "  *) exit 0 ;;\n"
                        "esac\n"
                    )
                    # delete_chv_network_state ends with
                    # `netfilter-persistent save`; never let a sandbox run
                    # rewrite the host's persisted iptables rules.
                    netfilter_persistent = bin_dir / "netfilter-persistent"
                    netfilter_persistent.write_text(
                        "#!/bin/bash\n"
                        f"printf 'netfilter-persistent %s\\n' \"$*\" >> {log}\n"
                    )
                    for tool in (ip, nft, iptables, netfilter_persistent):
                        tool.chmod(0o755)
                    script = (
                        "set -euo pipefail\n"
                        'cmd_exists() { command -v "$1" >/dev/null 2>&1; }\n'
                        'have() { command -v "$1" >/dev/null 2>&1; }\n'
                        "CHV_DB_PATH=/nonexistent.db\n"
                        "CHV_USER=chv-no-such-user\n"
                        "INSTALL_CHV_BRIDGE_NAME=chvbr0\n"
                        f"INSTALL_CHV_BRIDGE_CIDR={configured}\n"
                        f"CHV_HOST_ETC='{etc}'\n"
                        f"CHV_IPTABLES_RULES_V4='{rules}'\n"
                        f"{functions}\n"
                        "delete_chv_network_state\n"
                    )
                    subprocess.run(
                        ["bash", "-c", script],
                        env=dict(
                            os.environ,
                            PATH=f"{bin_dir}:{os.environ['PATH']}",
                        ),
                        check=True,
                        capture_output=True,
                        text=True,
                    )
                    calls = log.read_text()
                    # If the sandbox were missing the fake, the host's real
                    # netfilter-persistent would run and leave no log entry.
                    self.assertIn("netfilter-persistent save", calls)
                self.assertIn(
                    f"iptables -t nat -C POSTROUTING -s {expected} "
                    f"! -d {expected} -j MASQUERADE",
                    calls,
                )

    @unittest.skipUnless(shutil.which("bash"), "bash is required")
    def test_network_id_discovery_survives_failing_nft_and_ip(self):
        # A partially failing nft/ip must not truncate id discovery or abort
        # the teardown under bash pipefail. Whatever the tool printed before
        # exiting non-zero is still used.
        for relative in (
            "scripts/install.sh",
            "scripts/quick-install.sh",
            "packaging/scripts/preremove.sh",
        ):
            with self.subTest(script=relative):
                source = (ROOT / relative).read_text()
                functions = re.search(
                    r"^chv_network_ids\(\) \{.*?\n\}", source, re.S | re.M
                ).group(0)
                with tempfile.TemporaryDirectory() as tmp:
                    bin_dir = pathlib.Path(tmp) / "bin"
                    bin_dir.mkdir()
                    nft = bin_dir / "nft"
                    nft.write_text(
                        "#!/bin/bash\n"
                        'if [ "$1 $2" = "list tables" ]; then\n'
                        "  printf '%s\\n' 'table inet chv-deadbeef'\n"
                        "  exit 1\n"
                        "fi\n"
                    )
                    ip = bin_dir / "ip"
                    ip.write_text(
                        "#!/bin/bash\n"
                        'if [ "$1 $2" = "netns list" ]; then\n'
                        "  printf '%s\\n' ns-default\n"
                        "  exit 1\n"
                        "fi\n"
                    )
                    nft.chmod(0o755)
                    # A failing sort must not discard the ids it already
                    # received. Under pipefail it would otherwise abort
                    # preremove's `chv_network_ids | while` teardown.
                    sort_tool = bin_dir / "sort"
                    sort_tool.write_text(
                        "#!/bin/bash\n"
                        "cat\n"
                        "exit 1\n"
                    )
                    ip.chmod(0o755)
                    sort_tool.chmod(0o755)
                    script = (
                        "set -euo pipefail\n"
                        'cmd_exists() { command -v "$1" >/dev/null 2>&1; }\n'
                        'have() { command -v "$1" >/dev/null 2>&1; }\n'
                        "CHV_DB_PATH=/nonexistent.db\n"
                        "CHV_USER=chv-no-such-user\n"
                        f"{functions}\n"
                        "chv_network_ids\n"
                    )
                    result = subprocess.run(
                        ["bash", "-c", script],
                        env=dict(
                            os.environ,
                            PATH=f"{bin_dir}:{os.environ['PATH']}",
                        ),
                        check=True,
                        capture_output=True,
                        text=True,
                    )
                ids = result.stdout.split()
                self.assertIn("deadbeef", ids, relative)
                self.assertIn("default", ids, relative)

    def test_network_sweep_survives_failing_nft(self):
        # The final orphan-table sweep re-lists nft tables. If that listing
        # prints a row and then exits non-zero, pipefail would abort the whole
        # teardown before the table is deleted unless the pipeline is guarded.
        for relative in (
            "scripts/install.sh",
            "scripts/quick-install.sh",
            "packaging/scripts/preremove.sh",
        ):
            with self.subTest(script=relative):
                source = (ROOT / relative).read_text()
                functions = "\n".join(
                    re.search(rf"^{name}\(\) \{{.*?\n\}}", source, re.S | re.M).group(0)
                    for name in (
                        "chv_bridge_name_for_network",
                        "chv_network_ids",
                        "chv_network_cidr",
                        "remove_chv_bridge_and_nat",
                        "delete_chv_network_state",
                    )
                )
                with tempfile.TemporaryDirectory() as tmp:
                    tmp_path = pathlib.Path(tmp)
                    bin_dir = tmp_path / "bin"
                    bin_dir.mkdir()
                    log = tmp_path / "calls.log"
                    log.write_text("")
                    count = tmp_path / "nft-list.count"
                    etc = tmp_path / "etc"
                    (etc / "sysctl.d").mkdir(parents=True)
                    rules = tmp_path / "rules.v4"
                    rules.write_text("*nat\nCOMMIT\n")

                    nft = bin_dir / "nft"
                    nft.write_text(
                        "#!/bin/bash\n"
                        "printf 'nft %s\\n' \"$*\" >> " + str(log) + "\n"
                        'if [ "$1 $2" = "list tables" ]; then\n'
                        "  n=$(cat " + str(count) + " 2>/dev/null || echo 0)\n"
                        "  n=$((n + 1)); printf '%s\\n' \"$n\" > " + str(count) + "\n"
                        # First listing (id discovery) is empty and succeeds;
                        # the sweep listing prints a table and then fails.
                        '  if [ "$n" -ge 2 ]; then\n'
                        "    printf '%s\\n' 'table inet chv-orphan'\n"
                        "    printf '%s\\n' 'table ip filter'\n"
                        "    exit 1\n"
                        "  fi\n"
                        "fi\n"
                        "exit 0\n"
                    )
                    ip = bin_dir / "ip"
                    ip.write_text("#!/bin/bash\nexit 0\n")
                    iptables = bin_dir / "iptables"
                    iptables.write_text(
                        "#!/bin/bash\n"
                        "printf 'iptables %s\\n' \"$*\" >> " + str(log) + "\n"
                        'if [ "$3" = "-C" ]; then exit 1; fi\n'
                        "exit 0\n"
                    )
                    # The sweep runs the full teardown; never let it call
                    # the host's real netfilter-persistent.
                    netfilter_persistent = bin_dir / "netfilter-persistent"
                    netfilter_persistent.write_text(
                        "#!/bin/bash\n"
                        f"printf 'netfilter-persistent %s\\n' \"$*\" >> {log}\n"
                    )
                    for tool in (nft, ip, iptables, netfilter_persistent):
                        tool.chmod(0o755)

                    script = (
                        "set -euo pipefail\n"
                        'cmd_exists() { command -v "$1" >/dev/null 2>&1; }\n'
                        'have() { command -v "$1" >/dev/null 2>&1; }\n'
                        "CHV_DB_PATH=/nonexistent.db\n"
                        "CHV_USER=chv-no-such-user\n"
                        "INSTALL_CHV_BRIDGE_NAME=chvbr0\n"
                        "INSTALL_CHV_BRIDGE_CIDR=10.200.0.1/24\n"
                        f"CHV_HOST_ETC='{etc}'\n"
                        f"CHV_IPTABLES_RULES_V4='{rules}'\n"
                        f"{functions}\n"
                        "delete_chv_network_state\n"
                    )
                    subprocess.run(
                        ["bash", "-c", script],
                        env=dict(
                            os.environ,
                            PATH=f"{bin_dir}:{os.environ['PATH']}",
                        ),
                        check=True,
                        capture_output=True,
                        text=True,
                    )
                    calls = log.read_text()
                self.assertIn(
                    "nft delete table inet chv-orphan", calls, relative
                )
                self.assertIn("netfilter-persistent save", calls, relative)
                # A non-CHV table is never touched.
                self.assertNotIn("nft delete table inet filter", calls, relative)

    def _assert_preremove_upgrade_is_a_noop(self):
        # dpkg/rpm call preremove.sh with `upgrade` (or `1`) on upgrade. The
        # restarted agent adopts live VMs, so no service stop or host teardown
        # may run. Every command that could mutate the host is faked and the
        # env seams point at the sandbox, so a regression cannot touch the
        # test host before this assertion fails.
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            bin_dir = tmp_path / "bin"
            bin_dir.mkdir()
            log = tmp_path / "calls.log"
            log.write_text("")
            etc = tmp_path / "etc"
            (etc / "sysctl.d").mkdir(parents=True)
            rules = tmp_path / "rules.v4"
            rules.write_text("*nat\nCOMMIT\n")
            for name in (
                "ip",
                "nft",
                "systemctl",
                "kill",
                "ps",
                "rm",
                "mv",
                "chown",
                "iptables",
                "iptables-save",
                "netfilter-persistent",
                "sqlite3",
                "runuser",
                "sudo",
                "sleep",
                "id",
            ):
                tool = bin_dir / name
                tool.write_text(
                    "#!/bin/bash\n" f"printf '{name} %s\\n' \"$*\" >> {log}\n"
                )
                tool.chmod(0o755)
            env = dict(
                os.environ,
                PATH=f"{bin_dir}:{os.environ['PATH']}",
                CHV_HOST_ETC=str(etc),
                CHV_IPTABLES_RULES_V4=str(rules),
                CHV_DB_PATH=str(tmp_path / "nonexistent.db"),
            )
            for action in ("upgrade", "1"):
                subprocess.run(
                    ["sh", str(ROOT / "packaging/scripts/preremove.sh"), action],
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                )
            self.assertEqual(
                log.read_text(), "", "upgrade must not touch services or host state"
            )

    @unittest.skipUnless(shutil.which("awk"), "awk is required to exercise the matcher")
    def test_vmm_process_matchers_only_select_chv_owned_vmms(self):
        # The match must not signal an unrelated cloud-hypervisor on the host
        # (no CHV api-socket path) nor a shell whose command line merely
        # mentions the path. Run the shipped awk programs against synthetic
        # `ps -eo pid=,comm=,args=` output.
        ps_output = "\n".join(
            (
                "123 cloud-hyperviso /usr/bin/cloud-hypervisor --api-socket /var/lib/chv/agent/vms/aa/vm.sock",
                "124 cloud-hypervisor /opt/other/cloud-hypervisor --api-socket /tmp/x.sock",
                "125 bash bash -c ps -eo pid=,comm=,args= | grep /var/lib/chv/agent/vms/",
                "126 cloud-hyperviso /usr/bin/cloud-hypervisor --api-socket /var/lib/chv/agent/vms-other/bb/vm.sock",
                "127 cloud-hyperviso /usr/bin/cloud-hypervisor --api-socket /var/lib/chv/agent/vms/cc/vm.sock",
            )
        ) + "\n"
        programs = []
        for relative in (
            "scripts/install.sh",
            "scripts/quick-install.sh",
            "packaging/scripts/preremove.sh",
        ):
            text = (ROOT / relative).read_text()
            matches = [
                program
                for program in re.findall(r"awk '([^']*)'", text, re.S)
                if "cloud-hyperviso" in program
            ]
            self.assertTrue(matches, f"{relative}: no VMM matcher")
            programs.extend((relative, program) for program in matches)

        for relative, program in programs:
            result = subprocess.run(
                ["awk", program],
                input=ps_output,
                text=True,
                capture_output=True,
                check=True,
            )
            pids = sorted(line.split()[0] for line in result.stdout.splitlines() if line.strip())
            self.assertEqual(pids, ["123", "127"], f"{relative}: matcher selected {pids}")


if __name__ == "__main__":
    unittest.main()
