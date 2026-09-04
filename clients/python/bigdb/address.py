# Copyright 2026 Bany
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Where to connect, and whether that connection is encrypted.

# TLS is chosen by scheme and never guessed

`crates/big-bin/src/client/http.rs::transport` is the rule this ports: a bare `host:port` is
plaintext, `https://` is TLS, `http://` is plaintext, and any other scheme is refused by name.
Nothing is inferred from the port number - a client that guessed would connect in the clear to
a server it believed it had encrypted, which is the one failure mode a caller cannot see.
"""

from __future__ import annotations

import ipaddress
from dataclasses import dataclass

from .config import DEFAULT_ADDR
from .errors import ConfigError

__all__ = ["Address"]

#: Addresses whose plaintext is nobody else's business, so a credential over them is silent.
_LOOPBACK_NAMES = frozenset({"localhost", "localhost.localdomain"})


@dataclass(frozen=True, slots=True)
class Address:
    """One server, parsed once."""

    host: str
    port: int
    tls: bool
    ca_file: str | None = None
    insecure_skip_verify: bool = False

    @classmethod
    def parse(
        cls,
        addr: str = DEFAULT_ADDR,
        *,
        ca_file: str | None = None,
        insecure_skip_verify: bool = False,
    ) -> Address:
        """`host:port`, `http://host:port` or `https://host:port`, and nothing else."""
        rest, tls = _split_scheme(addr)
        if not tls and ca_file is not None:
            # The same refusal `--ca-file` gets there: it only means something with an https
            # address, and accepting it silently would suggest a verification that is not
            # happening.
            raise ConfigError("a ca_file only means something with an https:// address")
        host, port = _split_host_port(rest)
        return cls(
            host=host,
            port=port,
            tls=tls,
            ca_file=ca_file,
            insecure_skip_verify=insecure_skip_verify,
        )

    @property
    def authority(self) -> str:
        """What goes in the `Host` header, with IPv6 bracketed as a URL wants it."""
        host = f"[{self.host}]" if ":" in self.host else self.host
        return f"{host}:{self.port}"

    @property
    def is_loopback(self) -> bool:
        """Whether a password over this connection stays on this machine."""
        if self.host.lower() in _LOOPBACK_NAMES:
            return True
        try:
            return ipaddress.ip_address(self.host).is_loopback
        except ValueError:
            return False

    def __str__(self) -> str:
        return f"{'https' if self.tls else 'http'}://{self.authority}"


def _split_scheme(addr: str) -> tuple[str, bool]:
    """`(rest, wants_tls)`, refusing any scheme this client does not speak."""
    scheme, sep, rest = addr.partition("://")
    if not sep:
        return addr, False
    if scheme == "https":
        return rest, True
    if scheme == "http":
        return rest, False
    raise ConfigError(f"`{scheme}://` is not a scheme this client speaks; use https://")


def _split_host_port(rest: str) -> tuple[str, int]:
    """`host:port`, with a bracketed IPv6 host kept whole.

    A port is required rather than defaulted. `big serve` takes an address on the command line
    and a deployment is free to choose any port; defaulting one here would turn a typo into a
    connection to the wrong place.
    """
    rest = rest.rstrip("/")
    if not rest:
        raise ConfigError("an address with no host in it")

    if rest.startswith("["):
        host, close, tail = rest[1:].partition("]")
        if not close:
            raise ConfigError(f"{rest} opens a bracket it does not close")
        port_text = tail[1:] if tail.startswith(":") else ""
    else:
        host, colon, port_text = rest.rpartition(":")
        if not colon:
            raise ConfigError(f"{rest} names no port; write it as host:port")

    if not host:
        raise ConfigError(f"{rest} names no host")
    if not port_text.isdigit():
        raise ConfigError(f"{rest} names no port; write it as host:port")
    port = int(port_text)
    if not 1 <= port <= 65535:
        raise ConfigError(f"{port} is not a port number")
    return host, port
