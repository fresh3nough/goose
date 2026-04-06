from __future__ import annotations

import threading
from pathlib import Path
from typing import Any

import paramiko
from fastmcp import FastMCP

mcp = FastMCP("ssh-stateless")


def _build_connect_kwargs(
    host: str,
    username: str,
    password: str | None,
    key_path: str | None,
    port: int,
) -> dict[str, Any] | None:
    if password is None and key_path is None:
        return None

    connect_kwargs: dict[str, Any] = {
        "hostname": host,
        "port": port,
        "username": username,
        "allow_agent": False,
        "look_for_keys": False,
        "timeout": 15,
        "banner_timeout": 15,
        "auth_timeout": 15,
    }

    if password is not None:
        connect_kwargs["password"] = password
    else:
        connect_kwargs["key_filename"] = str(Path(key_path).expanduser())

    return connect_kwargs


def _read_stream(stream: Any) -> str:
    data = stream.read()
    if isinstance(data, bytes):
        return data.decode(errors="replace")
    return str(data)


@mcp.tool()
def ssh_exec(
    host: str,
    username: str,
    command: str,
    password: str | None = None,
    key_path: str | None = None,
    port: int = 22,
    insecure: bool = False,
) -> str:
    """Run a single SSH command over a fresh connection with no persisted session state.

    Args:
        host: SSH server hostname or IP address.
        username: Username for SSH authentication.
        command: Command to execute on the remote host.
        password: Password for authentication (provide password or key_path).
        key_path: Path to private key file (provide password or key_path).
        port: SSH port (default 22).
        insecure: If True, accept any host key (MITM risk). If False (default),
                  verify against system known_hosts and reject unknown keys.
    """
    connect_kwargs = _build_connect_kwargs(host, username, password, key_path, port)
    if connect_kwargs is None:
        return "Error: Provide password or key_path"

    client = paramiko.SSHClient()
    if insecure:
        client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    else:
        client.load_system_host_keys()
        client.set_missing_host_key_policy(paramiko.RejectPolicy())

    try:
        client.connect(**connect_kwargs)
        _, stdout, stderr = client.exec_command(command)

        # Drain both streams CONCURRENTLY to avoid deadlock: if we read
        # stdout first while stderr fills up, the remote process blocks
        # and we hang until timeout.
        results: dict[str, str] = {}

        def drain(name: str, stream: Any) -> None:
            results[name] = _read_stream(stream).strip()

        stdout_thread = threading.Thread(target=drain, args=("stdout", stdout))
        stderr_thread = threading.Thread(target=drain, args=("stderr", stderr))
        stdout_thread.start()
        stderr_thread.start()
        stdout_thread.join()
        stderr_thread.join()

        exit_code = stdout.channel.recv_exit_status()
        output_parts = [results.get("stdout", ""), results.get("stderr", "")]
        output = "\n".join(part for part in output_parts if part).strip()

        if exit_code != 0:
            return f"Error: Command exited with code {exit_code}\n{output}".strip()
        return output
    finally:
        client.close()


def main() -> None:
    mcp.run()
