from __future__ import annotations

import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from mcp_ssh_stateless.server import ssh_exec
import mcp_ssh_stateless.server as server_mod


class FakeChannel:
    def __init__(self, exit_status: int = 0) -> None:
        self._exit_status = exit_status

    def recv_exit_status(self) -> int:
        return self._exit_status


class FakeStream:
    def __init__(self, payload: bytes, channel: FakeChannel | None = None) -> None:
        self.payload = payload
        self.channel = channel or FakeChannel(0)

    def read(self) -> bytes:
        return self.payload


class FakeSSHClient:
    instances: list[FakeSSHClient] = []
    stdout_payload = b""
    stderr_payload = b""
    exit_status = 0

    def __init__(self) -> None:
        self.connect_calls: list[dict[str, object]] = []
        self.exec_calls: list[str] = []
        self.closed = False
        self.policy = None
        self.loaded_system_keys = False
        FakeSSHClient.instances.append(self)

    def load_system_host_keys(self) -> None:
        self.loaded_system_keys = True

    def set_missing_host_key_policy(self, policy: object) -> None:
        self.policy = policy

    def connect(self, **kwargs: object) -> None:
        self.connect_calls.append(kwargs)

    def exec_command(self, command: str) -> tuple[None, FakeStream, FakeStream]:
        self.exec_calls.append(command)
        channel = FakeChannel(FakeSSHClient.exit_status)
        return None, FakeStream(self.stdout_payload, channel), FakeStream(self.stderr_payload, channel)

    def close(self) -> None:
        self.closed = True


class SSHStatelessTests(unittest.TestCase):
    def setUp(self) -> None:
        FakeSSHClient.instances.clear()
        FakeSSHClient.stdout_payload = b"command output\n"
        FakeSSHClient.stderr_payload = b""
        FakeSSHClient.exit_status = 0

    def test_password_auth_runs_command_and_closes(self) -> None:
        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            result = ssh_exec(
                host="example.com",
                username="alice",
                command="whoami",
                password="secret",
            )

        self.assertEqual(result, "command output")
        self.assertEqual(len(FakeSSHClient.instances), 1)

        client = FakeSSHClient.instances[0]
        self.assertTrue(client.closed)
        self.assertEqual(client.exec_calls, ["whoami"])
        self.assertEqual(
            client.connect_calls,
            [
                {
                    "hostname": "example.com",
                    "port": 22,
                    "username": "alice",
                    "allow_agent": False,
                    "look_for_keys": False,
                    "timeout": 15,
                    "banner_timeout": 15,
                    "auth_timeout": 15,
                    "password": "secret",
                }
            ],
        )

    def test_key_auth_expands_key_path(self) -> None:
        expected_key_path = str(Path("~/.ssh/id_ed25519").expanduser())

        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            ssh_exec(
                host="example.com",
                username="alice",
                command="hostname",
                key_path="~/.ssh/id_ed25519",
                port=2200,
            )

        client = FakeSSHClient.instances[0]
        self.assertEqual(client.exec_calls, ["hostname"])
        self.assertEqual(client.connect_calls[0]["key_filename"], expected_key_path)
        self.assertEqual(client.connect_calls[0]["port"], 2200)
        self.assertTrue(client.closed)

    def test_missing_auth_returns_error_without_connecting(self) -> None:
        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            result = ssh_exec(
                host="example.com",
                username="alice",
                command="pwd",
            )

        self.assertEqual(result, "Error: Provide password or key_path")
        self.assertEqual(FakeSSHClient.instances, [])

    def test_each_tool_call_uses_a_new_client(self) -> None:
        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            ssh_exec(
                host="example.com",
                username="alice",
                command="date",
                password="one",
                insecure=True,
            )
            ssh_exec(
                host="example.com",
                username="alice",
                command="uptime",
                password="two",
                insecure=True,
            )

        self.assertEqual(len(FakeSSHClient.instances), 2)
        self.assertIsNot(FakeSSHClient.instances[0], FakeSSHClient.instances[1])
        self.assertTrue(all(client.closed for client in FakeSSHClient.instances))

    def test_nonzero_exit_code_returns_error(self) -> None:
        FakeSSHClient.exit_status = 1
        FakeSSHClient.stdout_payload = b""
        FakeSSHClient.stderr_payload = b"command not found\n"

        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            result = ssh_exec(
                host="example.com",
                username="alice",
                command="nonexistent",
                password="secret",
                insecure=True,
            )

        self.assertIn("Error: Command exited with code 1", result)
        self.assertIn("command not found", result)

    def test_secure_mode_loads_system_keys_and_rejects_unknown(self) -> None:
        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            with patch.object(server_mod.paramiko, "RejectPolicy") as mock_reject:
                mock_reject.return_value = "reject_policy_instance"
                ssh_exec(
                    host="example.com",
                    username="alice",
                    command="whoami",
                    password="secret",
                    insecure=False,
                )

        client = FakeSSHClient.instances[0]
        self.assertTrue(client.loaded_system_keys)
        self.assertEqual(client.policy, "reject_policy_instance")

    def test_insecure_mode_uses_auto_add_policy(self) -> None:
        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            with patch.object(server_mod.paramiko, "AutoAddPolicy") as mock_auto:
                mock_auto.return_value = "auto_add_policy_instance"
                ssh_exec(
                    host="example.com",
                    username="alice",
                    command="whoami",
                    password="secret",
                    insecure=True,
                )

        client = FakeSSHClient.instances[0]
        self.assertFalse(client.loaded_system_keys)
        self.assertEqual(client.policy, "auto_add_policy_instance")


if __name__ == "__main__":
    unittest.main()
