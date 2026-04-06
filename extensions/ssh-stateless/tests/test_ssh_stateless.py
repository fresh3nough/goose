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


class FakeStdin:
    def __init__(self) -> None:
        self.closed = False

    def close(self) -> None:
        self.closed = True


class FakeStream:
    def __init__(self, payload: bytes, channel: FakeChannel | None = None, raise_on_read: BaseException | None = None) -> None:
        self.payload = payload
        self.channel = channel or FakeChannel(0)
        self.raise_on_read = raise_on_read

    def read(self) -> bytes:
        if self.raise_on_read:
            raise self.raise_on_read
        return self.payload


class FakeSSHClient:
    instances: list[FakeSSHClient] = []
    stdout_payload = b""
    stderr_payload = b""
    exit_status = 0
    stdout_read_error: BaseException | None = None
    stderr_read_error: BaseException | None = None

    def __init__(self) -> None:
        self.connect_calls: list[dict[str, object]] = []
        self.exec_calls: list[str] = []
        self.closed = False
        self.policy = None
        self.loaded_system_keys = False
        self.last_stdin: FakeStdin | None = None
        FakeSSHClient.instances.append(self)

    def load_system_host_keys(self) -> None:
        self.loaded_system_keys = True

    def set_missing_host_key_policy(self, policy: object) -> None:
        self.policy = policy

    def connect(self, **kwargs: object) -> None:
        self.connect_calls.append(kwargs)

    def exec_command(self, command: str) -> tuple[FakeStdin, FakeStream, FakeStream]:
        self.exec_calls.append(command)
        channel = FakeChannel(FakeSSHClient.exit_status)
        stdin = FakeStdin()
        self.last_stdin = stdin
        stdout = FakeStream(self.stdout_payload, channel, FakeSSHClient.stdout_read_error)
        stderr = FakeStream(self.stderr_payload, channel, FakeSSHClient.stderr_read_error)
        return stdin, stdout, stderr

    def close(self) -> None:
        self.closed = True


class SSHStatelessTests(unittest.TestCase):
    def setUp(self) -> None:
        FakeSSHClient.instances.clear()
        FakeSSHClient.stdout_payload = b"command output\n"
        FakeSSHClient.stderr_payload = b""
        FakeSSHClient.exit_status = 0
        FakeSSHClient.stdout_read_error = None
        FakeSSHClient.stderr_read_error = None

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

    def test_key_with_passphrase_uses_both(self) -> None:
        expected_key_path = str(Path("~/.ssh/id_ed25519").expanduser())

        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            ssh_exec(
                host="example.com",
                username="alice",
                command="whoami",
                key_path="~/.ssh/id_ed25519",
                password="my-passphrase",
                insecure=True,
            )

        client = FakeSSHClient.instances[0]
        self.assertEqual(client.connect_calls[0]["key_filename"], expected_key_path)
        self.assertEqual(client.connect_calls[0]["passphrase"], "my-passphrase")
        self.assertNotIn("password", client.connect_calls[0])

    def test_stream_read_error_propagates(self) -> None:
        FakeSSHClient.stdout_read_error = IOError("Connection reset")

        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            result = ssh_exec(
                host="example.com",
                username="alice",
                command="whoami",
                password="secret",
                insecure=True,
            )

        self.assertIn("Error: Failed to read SSH output", result)
        self.assertIn("stdout", result)
        self.assertIn("Connection reset", result)
        self.assertTrue(FakeSSHClient.instances[0].closed)

    def test_stdin_is_closed_after_exec(self) -> None:
        with patch.object(server_mod.paramiko, "SSHClient", FakeSSHClient):
            ssh_exec(
                host="example.com",
                username="alice",
                command="cat",
                password="secret",
                insecure=True,
            )

        client = FakeSSHClient.instances[0]
        self.assertIsNotNone(client.last_stdin)
        self.assertTrue(client.last_stdin.closed)


if __name__ == "__main__":
    unittest.main()
