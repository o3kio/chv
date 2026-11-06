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
        ):
            self.assertIn(directive, packaged)
            self.assertIn(directive, standalone)
        self.assertIn("ExecStartPre=+/usr/bin/install -d -m 0700", standalone)
        self.assertNotIn("ExecStartPost=", standalone)

    def test_install_surfaces_provision_exact_private_parents(self):
        postinstall = (ROOT / "packaging/scripts/postinstall.sh").read_text()
        tmpfiles = (ROOT / "packaging/tmpfiles/chv-node.conf").read_text()
        for path in ("/var/lib/chv/agent", "/run/chv/core"):
            self.assertIn(path, postinstall)
            self.assertRegex(tmpfiles, rf"d {re.escape(path)} 0700 chv chv -")
        self.assertRegex(tmpfiles, r"d /run/chv/agent 0775 chv chv -")

    def test_no_packaging_surface_removes_core_socket_or_runtime_lease(self):
        surfaces = list((ROOT / "packaging").rglob("*")) + [ROOT / "scripts/install.sh"]
        forbidden = ("/run/chv/core/core-v1.sock", ".cellhv-runtime-authority.lease")
        for path in surfaces:
            if not path.is_file():
                continue
            text = path.read_text(errors="ignore")
            for token in forbidden:
                for line in text.splitlines():
                    if token in line and re.search(r"\b(?:rm|remove|unlink)\b", line):
                        self.fail(f"{path}: destructive Core path line: {line}")

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
        cases = [
            ("scripts/install.sh", shutil.which("bash")),
            ("scripts/quick-install.sh", shutil.which("bash")),
        ]
        posix_shell = shutil.which("dash") or shutil.which("sh")
        if posix_shell:
            cases.append(("packaging/scripts/preremove.sh", posix_shell))
        for relative, shell in cases:
            with self.subTest(script=relative):
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
                "br-63b276315a70\nbr-0b7f28f0\nbr-net1\nchvbr0\nens19\n"
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
                "'table inet chv-gone' 'table inet chvhs-deadbeef' 'table ip filter'\n"
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
            "nft delete table inet chv-63b27631c9c92fda4a43351c5aeeba21",
            "nft delete table inet chv-0b7f28f0",
            "nft delete table inet chv-default",
            "nft delete table inet chv-net1",
            "nft delete table inet chv-gone",
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
        self._assert_preremove_upgrade_is_a_noop()

    def _exercise_database_id_source(self, relative, shell):
        source = (ROOT / relative).read_text()
        functions = "\n".join(
            re.search(rf"^{name}\(\) \{{.*?\n\}}", source, re.S | re.M).group(0)
            for name in (
                "chv_bridge_name_for_network",
                "chv_network_ids",
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
            rules.write_text("")
            etc = tmp_path / "etc"
            etc.mkdir()

            # No nft tables and no namespaces are reported, so the only way
            # `dbonly` can be removed is the database row.
            ip = bin_dir / "ip"
            ip.write_text(
                "#!/bin/bash\n" f"printf 'ip %s\\n' \"$*\" >> {log}\n"
            )
            nft = bin_dir / "nft"
            nft.write_text(
                "#!/bin/bash\n" f"printf 'nft %s\\n' \"$*\" >> {log}\n"
            )
            sqlite3 = bin_dir / "sqlite3"
            sqlite3.write_text(
                "#!/bin/bash\n"
                f"printf 'sqlite3 %s\\n' \"$*\" >> {log}\n"
                "printf 'dbonly\\n'\n"
            )
            chown = bin_dir / "chown"
            chown.write_text(
                "#!/bin/bash\n" f"printf 'chown %s\\n' \"$*\" >> {log}\n"
            )
            for tool in (ip, nft, sqlite3, chown):
                tool.chmod(0o755)

            pipefail = "-o pipefail" if "bash" in (shell or "") else ""
            script = (
                f"set -eu {pipefail}\n".replace("  ", " ")
                + 'cmd_exists() { command -v "$1" >/dev/null 2>&1; }\n'
                'have() { command -v "$1" >/dev/null 2>&1; }\n'
                # The chv user does not exist in the sandbox, so both helper
                # spellings take the plain-sqlite3 fallback.
                'chv_sqlite() { sqlite3 "$@"; }\n'
                'chv_sqlite_read() { sqlite3 "$@"; }\n'
                f"CHV_DB_PATH='{db}'\n"
                "CHV_USER=chv-no-such-user\n"
                "INSTALL_CHV_BRIDGE_NAME=chvbr0\n"
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

        self.assertIn("sqlite3", calls, relative)
        self.assertIn("ip netns del ns-dbonly", calls, relative)
        self.assertIn("nft delete table inet chv-dbonly", calls, relative)
        self.assertIn("ip link del dev br-dbonly", calls, relative)
        if relative == "packaging/scripts/preremove.sh":
            # A root sqlite3 open can create root-owned WAL sidecars; the
            # preremove path hands them back to the service user.
            self.assertIn("chown chv:chv", calls, relative)
            self.assertIn("controlplane.db-wal", calls, relative)

    def _assert_preremove_upgrade_is_a_noop(self):
        # dpkg/rpm call preremove.sh with `upgrade` (or `1`) on upgrade. The
        # restarted agent adopts live VMs, so no service stop or host teardown
        # may run.
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            bin_dir = tmp_path / "bin"
            bin_dir.mkdir()
            log = tmp_path / "calls.log"
            log.write_text("")
            for name in ("ip", "nft", "systemctl", "kill", "ps"):
                tool = bin_dir / name
                tool.write_text(
                    "#!/bin/bash\n" f"printf '{name} %s\\n' \"$*\" >> {log}\n"
                )
                tool.chmod(0o755)
            env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}")
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
