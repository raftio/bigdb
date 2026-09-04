#!/bin/sh
# A peer CA, and one certificate per node signed by it.
#
# **This is what replaced the shared peer token.** Every node used to present the same `admin`
# string to every other, so one leaked credential was the whole cluster and there was no way to
# tell which node was speaking. A certificate carries a name: the CA signs one per node, the name
# in it has to be a name in cluster.toml, and revoking one node revokes one node.
#
# The names below are the `name` fields in cluster.toml, not the addresses. A certificate is
# issued to a name, and a node that moves keeps its name.
set -eu
cd "$(dirname "$0")"

nodes="a b a-spare"
days=${DAYS:-365}

mkdir -p secrets
chmod 700 secrets
cd secrets

# **With a name, this issues one more certificate and leaves everything else alone.** A node that
# joins a running cluster needs one, and the alternative - re-running the whole script - would
# rotate the CA and lock out the three nodes that are already talking. Called with no arguments it
# is the bootstrap it always was, and refuses to run twice for the same reason.
#
#   ./certs.sh          the CA and a certificate for each of the nodes above
#   ./certs.sh d        one more, signed by the CA that is already here
if [ "$#" -gt 0 ]; then
    [ -f peer-ca.key ] || {
        echo "no peer-ca.key here; the CA that signs a new certificate has to be the one the" >&2
        echo "cluster already trusts. Run ./certs.sh with no arguments to bootstrap one." >&2
        exit 1
    }
    nodes="$*"
    for node in $nodes; do
        [ -e "$node.pem" ] && {
            echo "$node.pem already exists; delete it first if you mean to reissue it" >&2
            exit 1
        }
    done
else
    if [ -f peer-ca.pem ]; then
        echo "peer-ca.pem already exists; delete secrets/ first if you mean to rotate," >&2
        echo "or name a node - \`./certs.sh d\` - to issue one more against this CA" >&2
        exit 1
    fi

    # The CA. Kept here so a node can be added later; in anything larger than a demo it belongs
    # somewhere the nodes cannot reach.
    openssl req -x509 -newkey ed25519 -nodes -days "$days" \
        -keyout peer-ca.key -out peer-ca.pem \
        -subj '/CN=big peer ca' 2>/dev/null
    chmod 600 peer-ca.key
fi

for node in $nodes; do
    # `subjectAltName` is what is actually checked - a CN is not, by anything current - and it
    # has to be the node's name from cluster.toml.
    openssl req -newkey ed25519 -nodes \
        -keyout "$node.key" -out "$node.csr" \
        -subj "/CN=$node" 2>/dev/null
    openssl x509 -req -in "$node.csr" -days "$days" \
        -CA peer-ca.pem -CAkey peer-ca.key -CAcreateserial \
        -extfile /dev/stdin -out "$node.pem" 2>/dev/null <<EXT
subjectAltName = DNS:$node
# Both, because each node is a server to its peers and a client to them at the same time.
extendedKeyUsage = serverAuth, clientAuth
EXT
    rm -f "$node.csr"
    chmod 600 "$node.key"
    chmod 644 "$node.pem"
done

rm -f peer-ca.srl
echo "wrote a certificate for each of: $nodes"
echo "keys are mode 600; certificates are public and are not"
echo
echo "Each node is started with this pair twice over: --peer-cert/--peer-key is what it presents"
echo "when it dials another node, and --tls-cert/--tls-key is what its own listener presents when"
echo "it is dialled. One listener serves peers and people both, so it is one certificate."
