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

"""Which failures are worth sending again, and after how long.

One pure function. It does not sleep, does not read a clock and does not know which transport
called it - which is what lets the sync loop and the async loop share a policy instead of
having two that agree until one is edited.

# The table, and where each row comes from

**`NotSent` - always.** Nothing was sent, so the server holds fewer bytes than
`Content-Length` promised and cannot parse a statement from them.

**A retryable `ServerError` - always.** A 503 is a refusal with nothing written: `big_http::shed`
answers it from the accepting thread without ever giving the body to a worker.

**`Unknown` and `ProtocolError` - only on an idempotent route.** Written in full, outcome
unknown, indistinguishable from a server that committed and then died.

**Any other `ServerError` - never.** The server understood it and said no; it will say no again.

The third row is the whole reason `Op.idempotent` exists. For `/table/{t}/import` a fact is a
bit set at a caller-chosen address, so sending a chunk twice writes the same bits twice, which
is writing them once. For an allocating `INSERT` over `/sql` it writes two records - so retrying
turns "possibly written once" into "possibly written twice", which is worse to be unsure about.
"""

from __future__ import annotations

from dataclasses import dataclass

from .config import DEFAULT_FIRST_BACKOFF, DEFAULT_MAX_BACKOFF, DEFAULT_RETRIES
from .errors import NotSent, ProtocolError, ServerError, Unknown

__all__ = ["RetryPolicy", "decide"]


@dataclass(frozen=True, slots=True)
class RetryPolicy:
    """How many more times, and how long to wait between."""

    retries: int = DEFAULT_RETRIES
    first_backoff: float = DEFAULT_FIRST_BACKOFF
    max_backoff: float = DEFAULT_MAX_BACKOFF


def decide(
    policy: RetryPolicy,
    attempt: int,
    failure: BaseException,
    *,
    idempotent: bool,
) -> float | None:
    """Seconds to wait before the next attempt, or `None` to give up and raise.

    `attempt` is 0 for the first try, so a policy of 3 retries allows attempts 0, 1, 2 and 3.
    """
    if attempt >= policy.retries:
        return None
    if not _worth_repeating(failure, idempotent=idempotent):
        return None

    # Exponential, capped. Doubling from a small first wait is what `big-message`'s producer
    # does, and the cap is there because a client that waits a minute has stopped being a
    # client and become an outage.
    delay = min(policy.first_backoff * (2**attempt), policy.max_backoff)

    # `Retry-After` is a floor, not a suggestion: the server said how long it needs, and
    # coming back sooner is asking to be shed again. It is allowed to exceed `max_backoff`
    # for that reason.
    if isinstance(failure, ServerError) and failure.retry_after is not None:
        delay = max(delay, float(failure.retry_after))
    return float(delay)


def _worth_repeating(failure: BaseException, *, idempotent: bool) -> bool:
    if isinstance(failure, NotSent):
        return True
    if isinstance(failure, ServerError):
        return failure.retryable
    if isinstance(failure, (Unknown, ProtocolError)):
        return idempotent
    # Anything else - a value this client refused, a body past the cap, a bad address - is a
    # thing that will happen again identically. Retrying it only delays the report.
    return False
