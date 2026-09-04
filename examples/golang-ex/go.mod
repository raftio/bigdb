// The example builds against the client in this repository rather than against a published
// version of it, so a change to clients/go is exercised here before it is tagged.
//
// A separate module rather than a package inside clients/go: `go get` on the client must not
// drag an example's main package into somebody's build, and a nested module is how Go says so.
module github.com/raftio/bigdb/examples/golang-ex

go 1.23

require github.com/raftio/bigdb/clients/go v0.0.0

replace github.com/raftio/bigdb/clients/go => ../../clients/go
