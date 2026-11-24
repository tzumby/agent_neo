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
