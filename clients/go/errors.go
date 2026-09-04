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
	"errors"
	"fmt"
	"time"
)

// One error tree, two ways in.
//
// A caller who wants to branch quickly writes errors.Is(err, ErrNotFound). A caller who wants
// the stable code writes errors.As(err, &se) and reads se.Code. Both reach the same object -
// nothing here is translated and nothing is re-raised, so a stack trace names what happened
// rather than what a translation layer decided it resembled.
//
// # Why the mapping is on code and never on prose
//
// big_http::json::error writes {"error": <sentence>, "code": <code>}. The sentence is written
// for a person and is improved when somebody finds a better one; the code is the stable half.
// A client that matched on prose would break when the prose got better.

// Sentinels. These are targets for errors.Is, not values that are ever returned on their own.
var (
	// ErrNotSent means the request provably never reached the server. Safe to send again.
	ErrNotSent = errors.New("bigdb: the request was not sent")
	// ErrUnknown means the request was written in full and what happened next is not known.
	// Never safe to send again unless the operation is idempotent.
	ErrUnknown = errors.New("bigdb: the outcome of the request is not known")

	ErrBadRequest      = errors.New("bigdb: bad request")
	ErrUnauthenticated = errors.New("bigdb: unauthenticated")
	ErrForbidden       = errors.New("bigdb: forbidden")
	ErrNotFound        = errors.New("bigdb: not found")
	ErrConflict        = errors.New("bigdb: conflict")
	ErrPayloadTooLarge = errors.New("bigdb: payload too large")
	ErrUnprocessable   = errors.New("bigdb: unprocessable")
	ErrClientClosed    = errors.New("bigdb: the client closed the request")
	ErrServerFault     = errors.New("bigdb: server fault")
	// ErrPartiallyApplied is its own sentinel because catching it is the repair path: some of
	// the write landed. Never retried - resending turns one thing to check into two.
	ErrPartiallyApplied = errors.New("bigdb: partially applied")
	ErrNotSupported     = errors.New("bigdb: not supported")
	ErrBadGateway       = errors.New("bigdb: bad gateway")
	ErrUnavailable      = errors.New("bigdb: unavailable")
	ErrQueryTimeout     = errors.New("bigdb: query timeout")
)

// ServerError is an answer from the server: it read the request, decided, and said so.
//
// Status and Code are the two halves worth branching on. Message is for a person. RequestID
// matters most on a 5xx, where the body is redacted (see big_http::status) and correlating
// against the server's log is the only thing left to do - which is why Error prints it first.
type ServerError struct {
	Status     int
	Code       string
	Message    string
	RequestID  string
	RetryAfter time.Duration
}

func (e *ServerError) Error() string {
	id := e.RequestID
	if id == "" {
		id = "-"
	}
	return fmt.Sprintf("bigdb: [%s] %d %s: %s", id, e.Status, e.Code, e.Message)
}

// Retryable reports whether sending this request again could succeed.
//
// Status 503 is the safety wire: a code this build has never heard of that arrives with a 503
// is still a node saying "not me, not now".
func (e *ServerError) Retryable() bool {
	if e.Status == 503 {
		return true
	}
	_, ok := retryableCodes[e.Code]
	return ok
}

// Is maps this error onto a sentinel, so errors.Is and errors.As are two views of one fact.
//
// The code table wins over the status table for the few codes whose meaning a status cannot
// carry: partially_applied is a 500 that must not be treated like any other 500.
func (e *ServerError) Is(target error) bool {
	if s, ok := byCode[e.Code]; ok {
		return s == target
	}
	return byStatus[e.Status] == target
}

// The codes big_cluster::ClusterError::code emits for a failure that is about where the work
// went rather than about the work, plus the two the HTTP layer raises for the same reason.
// Every one of these is "ask again, possibly somewhere else".
var retryableCodes = map[string]struct{}{
	"stale_route":               {},
	"range_moving":              {},
	"owner_unreachable":         {},
	"not_serving":               {},
	"schema_leader_unreachable": {},
	"server_busy":               {},
	"busy_authenticating":       {},
}

// Status to sentinel. The primary table: a code this build has never heard of still lands
// somewhere sensible, because the status is never absent.
var byStatus = map[int]error{
	400: ErrBadRequest,
	401: ErrUnauthenticated,
	403: ErrForbidden,
	404: ErrNotFound,
	409: ErrConflict,
	413: ErrPayloadTooLarge,
	422: ErrUnprocessable,
	499: ErrClientClosed,
	500: ErrServerFault,
	501: ErrNotSupported,
	502: ErrBadGateway,
	503: ErrUnavailable,
	504: ErrQueryTimeout,
}

// Code to sentinel, for the few codes whose sentinel the status cannot imply. Deliberately
// small - anything longer would be this client keeping a second copy of the server's error
// table, and the copy would go stale.
var byCode = map[string]error{
	"partially_applied": ErrPartiallyApplied,
	"request_too_large": ErrPayloadTooLarge,
}

// TransportError is a failure that happened on the wire, split by the only question that
// matters: did the server see the request?
//
// Two fields' worth of meaning in one bool, because there are exactly two things a caller can
// do. This is contrib/big-message's Failure enum, ported with its reasoning:
//
//   - Sent is false for everything up to and including the write and flush. The server holds
//     fewer bytes than Content-Length promised, and big_http::Request::read calls read_exact on
//     the declared length - so it cannot parse a statement from them and will never run one.
//     Retrying is provably safe.
//   - Sent is true for everything from reading the status line onward. A read timeout, a reset
//     and an EOF where a response should be are identical whether the server died before
//     running the statement or after committing it. There is no safe recovery, so it is
//     retried only for an operation that is idempotent anyway.
type TransportError struct {
	Sent   bool
	Op     string
	Target string
	Err    error
}

func (e *TransportError) Error() string {
	what := "was not sent"
	if e.Sent {
		what = "was sent and its outcome is not known"
	}
	return fmt.Sprintf("bigdb: %s %s %s: %v", e.Op, e.Target, what, e.Err)
}

func (e *TransportError) Unwrap() error { return e.Err }

func (e *TransportError) Is(target error) bool {
	if e.Sent {
		return target == ErrUnknown
	}
	return target == ErrNotSent
}

// ProtocolError is something that came back which is not an HTTP response this client reads.
//
// Its own type because it says something different about the deployment - a proxy in the path,
// most likely - than a connection that failed. The outcome is as unknown as a TransportError
// with Sent set, and it is treated the same way everywhere above.
type ProtocolError struct {
	What string
}

func (e *ProtocolError) Error() string { return "bigdb: " + e.What }

func (e *ProtocolError) Is(target error) bool { return target == ErrUnknown }

// ConfigError is a client that was asked for something it cannot be.
type ConfigError struct{ What string }

func (e *ConfigError) Error() string { return "bigdb: " + e.What }

// ValueError is a value that has no spelling in this dialect. Raised before any socket opens.
type ValueError struct{ What string }

func (e *ValueError) Error() string { return "bigdb: " + e.What }

// TooLargeError is a body this client refuses to send, measured before the socket opens.
//
// Line is which line of a fact body pushed it over, and is zero when the body has no lines.
// The server's own Error::MessageTooLarge carries the same thing for the same reason: a
// producer that sent a million needs to know which one, not that there was one.
type TooLargeError struct {
	Bytes int
	Cap   int
	Line  int
}

func (e *TooLargeError) Error() string {
	if e.Line > 0 {
		return fmt.Sprintf(
			"bigdb: the body reached %d bytes at line %d, over the %d this client sends",
			e.Bytes, e.Line, e.Cap)
	}
	return fmt.Sprintf("bigdb: the body is %d bytes, over the %d this client sends", e.Bytes, e.Cap)
}

func (e *TooLargeError) Is(target error) bool { return target == ErrPayloadTooLarge }
