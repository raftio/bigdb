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

if [ -f peer-ca.pem ]; then
    echo "peer-ca.pem already exists; delete secrets/ first if you mean to rotate" >&2
    exit 1
fi

# The CA. Kept here so a node can be added later; in anything larger than a demo it belongs
# somewhere the nodes cannot reach.
openssl req -x509 -newkey ed25519 -nodes -days "$days" \
    -keyout peer-ca.key -out peer-ca.pem \
    -subj '/CN=big peer ca' 2>/dev/null
chmod 600 peer-ca.key

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
echo "wrote secrets/peer-ca.pem and a certificate for each of: $nodes"
echo "keys are mode 600; certificates are public and are not"
