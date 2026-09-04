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

"""The only asymmetric code in this package: sockets, twice.

`base` holds the seam - `RawResponse`, and the header helpers both sides need. `sync` and `aio`
are the two implementations, and `tests/test_transport_conformance.py` runs one table of cases
against both so they cannot drift apart.
"""

from .base import RawResponse, Transport, basic

__all__ = ["RawResponse", "Transport", "basic"]
