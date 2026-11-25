"""Exercise the root helper's input and state transitions without using sudo."""

import json
from pathlib import Path
import subprocess
import tempfile
import types
import unittest
from unittest.mock import patch

from jinja2 import Environment, StrictUndefined
import yaml


ROLE = Path(__file__).resolve().parents[1] / "roles" / "agentd"
UNIT = "agentd-cmd-123456789abc"


def helper():
    defaults = yaml.safe_load((ROLE / "defaults" / "main.yml").read_text())
    environment = Environment(undefined=StrictUndefined)
    environment.filters["to_json"] = json.dumps
    template = (ROLE / "templates" / "privileged.py.j2").read_text()
    module = types.ModuleType("privileged")
    exec(compile(environment.from_string(template).render(defaults), "privileged.py", "exec"), module.__dict__)
    return module


class PrivilegedHelperTests(unittest.TestCase):
    def setUp(self):
        self.helper = helper()
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)

    def test_runner_arguments_keep_credentials_off_the_command_line(self):
        request = {
            "unit": UNIT, "command": "printf test", "seconds": 10, "route": "proxy",
            "keys": {"API_TOKEN": "mock-credential-value"},
        }
        self.helper.validate_run(request)
        args = self.helper.runner_args(request, self.path)
        self.assertIn("--uid=worker", args)
        self.assertIn("--property=BindsTo=neo-agent-guard.service agentd.service", args)
        self.assertIn("--setenv=PATH=/usr/sbin:/usr/bin:/sbin:/bin", args)
        self.assertEqual(args[-5:-1], ["/usr/bin/bash", "--noprofile", "--norc", "-c"])
        self.assertNotIn("mock-credential-value", " ".join(args))
        self.assertEqual((self.path / "API_TOKEN").read_text(), "mock-credential-value")
        self.assertEqual((self.path / "API_TOKEN").stat().st_mode & 0o777, 0o600)
        self.assertIn("--setenv=NO_PROXY=localhost,127.0.0.1,::1", args)
        self.assertIn("--setenv=no_proxy=localhost,127.0.0.1,::1", args)
        direct = self.helper.runner_args(dict(request, route="direct", keys={}), self.path)
        self.assertIn("--slice=neodirect.slice", direct)
        self.assertFalse(any(arg.startswith(("--setenv=NO_PROXY", "--setenv=HTTPS_PROXY")) for arg in direct))
        with self.assertRaises(ValueError):
            self.helper.validate_run(dict(request, route="direct"))
        with self.assertRaises(ValueError):
            self.helper.validate_run(dict(request, unit="squid"))

    def test_early_stop_cancels_a_pending_launch(self):
        self.helper.RUNTIME_DIR = self.path
        self.helper.WORKER_STATE = self.path / "worker"
        with patch.object(self.helper, "loaded", return_value=False):
            self.assertEqual(self.helper.stop_worker(UNIT), 0)
        self.assertTrue((self.helper.WORKER_STATE / UNIT).exists())
        request = {"unit": UNIT, "command": "printf hello", "seconds": 10, "route": "proxy", "keys": {}}
        with (
            patch.object(self.helper, "read_input", return_value=json.dumps(request)),
            patch.object(self.helper.pwd, "getpwnam", return_value=types.SimpleNamespace(pw_uid=1001)),
            patch.object(self.helper, "active", return_value=True),
            patch.object(self.helper.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)),
            patch.object(self.helper.subprocess, "Popen") as launch,
        ):
            self.assertEqual(self.helper.run_worker(), 1)
            launch.assert_not_called()

    def test_proxy_restarts_only_when_applied_policy_needs_reconciliation(self):
        self.helper.ALLOWED_HOSTS = self.path / "hosts"
        self.helper.APPLIED = self.path / "applied"
        commands = []

        def systemctl(*args):
            commands.append(args)
            code = int(args[0] == "start" and not self.helper.ALLOWED_HOSTS.is_file())
            return subprocess.CompletedProcess(args, code)

        with (
            patch.object(self.helper, "read_input", return_value="api.example.com"),
            patch.object(self.helper, "systemctl", side_effect=systemctl),
            patch.object(self.helper, "write_hosts", side_effect=self.helper.ALLOWED_HOSTS.write_text),
            patch.object(self.helper, "PROXY_LOCK", self.path / "proxy.lock"),
        ):
            self.assertEqual(self.helper.apply_proxy(), 0)
            self.assertEqual(commands[:2], [("stop", "squid.service"), ("start", "squid.service")])
            commands.clear()
            self.assertEqual(self.helper.apply_proxy(), 0)
            self.assertEqual(commands, [("is-active", "--quiet", "squid.service")])
            self.helper.APPLIED.unlink()
            commands.clear()
            self.assertEqual(self.helper.apply_proxy(), 0)
            self.assertEqual(commands[:2], [("stop", "squid.service"), ("start", "squid.service")])


if __name__ == "__main__":
    unittest.main()


class InstallSnapTests(unittest.TestCase):
    """install-snap: a strictly confined app from a verified publisher, the
    one the user approved, and its launchers placed as the desktop user."""

    def setUp(self):
        self.helper = helper()
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.launchers = Path(self.directory.name)
        self.helper.SNAP_LAUNCHERS = self.launchers
        self.publisher = "GKq9csPRQp1E5kUglZG9QTqDEBLrcszO"

    def store(self, **overrides):
        snap = {"name": "slack", "type": "app", "confinement": "strict", "version": "4.53.292",
                "publisher": {"id": self.publisher, "validation": "verified"}}
        snap.update(overrides)
        return snap

    def test_only_strict_apps_from_verified_publishers(self):
        cases = [
            (self.store(), True),
            (self.store(confinement="classic"), False),
            (self.store(confinement="devmode"), False),
            (self.store(type="os"), False),
            (self.store(publisher={"id": self.publisher, "validation": "unproven"}), False),
            (self.store(name="slack-unofficial"), False),
        ]
        for snap, allowed in cases:
            with patch.object(self.helper, "snapd_get", return_value=(200, {"result": [snap]})):
                if allowed:
                    self.assertEqual(self.helper.store_snap("slack")["version"], "4.53.292")
                else:
                    with self.assertRaises(ValueError, msg=str(snap)):
                        self.helper.store_snap("slack")

    def test_requests_are_checked_before_anything_runs(self):
        good = {"name": "slack", "publisher": self.publisher}
        self.assertEqual(self.helper.validate_install(good), ("slack", self.publisher))
        for bad in [
            dict(good, name="Slack"), dict(good, name="-slack"), dict(good, name="slack--x"),
            dict(good, name="123"), dict(good, name="a" * 41), dict(good, name="slack; rm -rf /"),
            dict(good, publisher="x"), dict(good, extra=1), {"name": "slack"}, ["slack"],
        ]:
            with self.assertRaises(ValueError, msg=str(bad)):
                self.helper.validate_install(bad)

    def test_installs_the_approved_publishers_snap_and_places_its_launchers(self):
        (self.launchers / "slack_slack.desktop").write_text("[Desktop Entry]\nName=Slack\n")
        (self.launchers / "slack_helper.desktop").write_text("[Desktop Entry]\nNoDisplay=true\n")
        (self.launchers / "slackware_x.desktop").write_text("[Desktop Entry]\nName=Other\n")
        calls = []

        def run(args, **kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 0)

        def snapd(path):
            if path.startswith("/v2/find"):
                return 200, {"result": [self.store()]}
            return 200, {"result": {"version": "4.53.292"}}

        request = json.dumps({"name": "slack", "publisher": self.publisher}).encode()
        stdin = types.SimpleNamespace(buffer=types.SimpleNamespace(read=lambda limit: request))
        out = []
        with patch.object(self.helper, "snapd_get", side_effect=snapd), \
                patch.object(self.helper.subprocess, "run", side_effect=run), \
                patch.object(self.helper.sys, "stdin", stdin), \
                patch.object(self.helper.pwd, "getpwnam", return_value=types.SimpleNamespace(pw_dir="/home/agent")), \
                patch("builtins.print", side_effect=lambda text: out.append(text)):
            self.assertEqual(self.helper.install_snap(), 0)
        self.assertEqual(calls[0], ["/usr/bin/snap", "install", "slack"])
        # Launchers go on the desktop as the desktop user, never as root, in a
        # unit of their own (agentd's sandbox hides /home from this helper).
        self.assertTrue(all(call[0] == "/usr/bin/systemd-run" and "--uid=agent" in call for call in calls[1:]), calls)
        self.assertEqual(calls[-1][-2:], ["/usr/bin/update-desktop-database", "/home/agent/.local/share/applications"])
        installed = [call[-1] for call in calls if "/usr/bin/install" in call]
        self.assertEqual(
            installed,
            ["/home/agent/Desktop/slack_slack.desktop", "/home/agent/.local/share/applications/slack_slack.desktop"],
        )
        self.assertEqual(json.loads(out[0]), {"name": "slack", "version": "4.53.292", "launchers": ["slack_slack.desktop"]})

    def test_another_publishers_snap_is_refused(self):
        request = json.dumps({"name": "slack", "publisher": "A" * 32}).encode()
        stdin = types.SimpleNamespace(buffer=types.SimpleNamespace(read=lambda limit: request))
        with patch.object(self.helper, "snapd_get", return_value=(200, {"result": [self.store()]})), \
                patch.object(self.helper.subprocess, "run") as run, \
                patch.object(self.helper.sys, "stdin", stdin):
            with self.assertRaises(ValueError):
                self.helper.install_snap()
        run.assert_not_called()


class SudoersTests(unittest.TestCase):
    """The sudoers file as Ansible renders it (trim_blocks on): install-snap
    only when installs are on, and every rule on its own line."""

    def render(self, installs):
        defaults = yaml.safe_load((ROLE / "defaults" / "main.yml").read_text())
        environment = Environment(undefined=StrictUndefined, trim_blocks=True)
        environment.filters["bool"] = lambda value: value is True or str(value).lower() in ("true", "yes", "on", "1")
        template = environment.from_string((ROLE / "templates" / "sudoers.j2").read_text())
        return template.render(defaults, ansible_managed="Ansible managed", agentd_app_installs=installs)

    def test_install_snap_only_when_installs_are_on(self):
        for installs in (True, False):
            lines = [line for line in self.render(installs).splitlines() if line and not line.startswith("#")]
            self.assertEqual(len(lines), 2, lines)
            self.assertTrue(lines[0].startswith("Cmnd_Alias NEO_AGENT_HELPER = "))
            self.assertEqual(lines[1], "agentd ALL=(root) NOPASSWD: NEO_AGENT_HELPER")
            self.assertEqual(lines[0].endswith("neo-agent-privileged install-snap"), installs)
