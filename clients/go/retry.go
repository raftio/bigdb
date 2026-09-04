// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package bigdb

import (
	"context"
	"errors"
	"time"
)

// The retry policy, as a pure function. No clock, no sleeping, no socket - so the table below
// can be asserted exhaustively in a test that runs in microseconds.

// decision is what to do after one attempt.
type decision struct {
	Retry bool
	Wait  time.Duration
}

// decide answers whether to send this request again.
//
//	failure                                  retry?
//	---------------------------------------  ------------------------------------------
//	TransportError{Sent: false}              yes, up to retries, for any operation
//	TransportError{Sent: true}, Protocol     only when the operation is idempotent
//	ServerError.Retryable() (503, and the    yes, for any operation; wait at least as
//	  cluster's "ask elsewhere" codes)         long as Retry-After asked
//	any other ServerError                    never
//
// A ServerError that is not retryable is the server having read the request and decided. Asking
// the same refused question again turns a clear failure into a confusing one.
//
// query_timeout (504) is deliberately not retryable: a query that has already run out of time
// will run out of time again, and the second one costs the server as much as the first.
//
// partially_applied (500) is not retryable either, and has its own sentinel so that
// errors.Is(err, ErrPartiallyApplied) is the repair path. Some of the write landed; sending it
// again turns one thing to check into two.
func decide(err error, idempotent bool, attempt, retries int, delay func(int) time.Duration) decision {
	if attempt >= retries {
		return decision{}
	}

	// The caller said stop. Nothing below this line is a reason to keep going, and retrying
	// after a cancellation would be the client deciding it knew better.
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return decision{}
	}

	wait := delay(attempt)

	var te *TransportError
	if errors.As(err, &te) {
		if !te.Sent {
			return decision{Retry: true, Wait: wait}
		}
		return decision{Retry: idempotent, Wait: wait}
	}

	var pe *ProtocolError
	if errors.As(err, &pe) {
		// As unknown an outcome as a half-finished exchange, and treated the same way.
		return decision{Retry: idempotent, Wait: wait}
	}

	var se *ServerError
	if errors.As(err, &se) {
		if !se.Retryable() {
			return decision{}
		}
		// Retry-After is a floor, not a replacement: a server that asked for two seconds gets
		// at least two, and the backoff still applies when it asked for less.
		if se.RetryAfter > wait {
			wait = se.RetryAfter
		}
		return decision{Retry: true, Wait: wait}
	}

	// Anything else is this client's own refusal - a value with no spelling, a body over the
	// ceiling, an address that will not parse. None of those get better on a second try.
	return decision{}
}
