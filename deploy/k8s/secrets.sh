#!/bin/sh
# Everything this deployment needs on disk, and then in three Secrets.
#
# **A CA of its own, not `deploy/cluster`'s.** Two deployments sharing one CA is two deployments
# that can impersonate each other, and this one's node names are a StatefulSet's - `big-0`,
# `big-1` - rather than `a` and `b`. So it points the two scripts that already exist at a
# directory of its own; there is no cryptography here.
#
#   ./secrets.sh              # generate what is missing, then upload
#   MAX=16 ./secrets.sh       # room for sixteen pods rather than eight
#
# **Certificates are issued to the ceiling, not to the current size**, and that is safe in a way
# a membership list is not: a certificate names a node, and a node is let in by the *roster*,
# which follows the agreement rather than this directory. An unused key grants nothing. What it
# buys is that `kubectl scale big --replicas=6` needs no new certificate, no `certs.sh` run and
# no restart of anybody.
set -eu
cd "$(dirname "$0")"

ns=${NS:-big}
out=${SECRETS:-secrets}
max=${MAX:-8}
big=${BIG:-big}

command -v kubectl >/dev/null || { echo "no \`kubectl\` on PATH" >&2; exit 1; }

# `big-0 big-1 ... big-<max-1>`: the pod names a StatefulSet called `big` will hand out.
nodes=""
i=0
while [ "$i" -lt "$max" ]; do
    nodes="$nodes big-$i"
    i=$((i + 1))
done
nodes=${nodes# }

# ---------------------------------------------------------------------------------------------
# On disk
# ---------------------------------------------------------------------------------------------

if [ ! -f "$out/users" ]; then
    command -v "$big" >/dev/null || {
        echo "no \`$big\` on PATH; set BIG=/path/to/big. Hashing a password is argon2 and a" >&2
        echo "shell cannot do it - see ../cluster/users.sh." >&2
        exit 1
    }
    SECRETS="../k8s/$out" ../cluster/users.sh
    # **A third credential, and the reason it is not `ops`.** A pod that registers itself needs
    # to reach `/admin/cluster/add-node`, which is `OPERATE ON *.*` - not superuser, and nothing
    # else. Handing every pod the superuser password to save one `GRANT` would put full SQL
    # access in a file every node can read.
    #
    # Named before the role exists, which is the right order and the same one `users.sh` uses
    # for `dashboard`: a role is resolved by name on every request, so `CREATE ROLE joiner`
    # after the cluster is up makes this credential work with no restart. Until then it
    # authenticates and may do nothing - so joining does not work until an operator has decided
    # it may.
    joiner=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
    printf '%s\n' "$joiner" | "$big" passwd "$out/users" set joiner --role joiner
    printf 'joiner:%s\n' "$joiner" > "$out/join-credentials"
    chmod 600 "$out/join-credentials"
    echo
    echo "  joiner:    $joiner   (role \`joiner\`, which does not exist yet)"
fi

if [ ! -f "$out/peer-ca.pem" ]; then
    NODES="$nodes" SECRETS="../k8s/$out" ../cluster/certs.sh
else
    # Already bootstrapped: issue only the names that are missing, against the CA that is there.
    missing=""
    for node in $nodes; do
        [ -f "$out/$node.pem" ] || missing="$missing $node"
    done
    if [ -n "$missing" ]; then
        # shellcheck disable=SC2086
        SECRETS="../k8s/$out" ../cluster/certs.sh $missing
    fi
fi

# ---------------------------------------------------------------------------------------------
# In the cluster
# ---------------------------------------------------------------------------------------------
#
# `create --dry-run=client | apply` rather than plain `create`: run twice, this rotates what is
# there instead of failing on the second run. A pod already running keeps what it staged at
# startup until it restarts - these files are read once.

kubectl get namespace "$ns" >/dev/null 2>&1 || kubectl create namespace "$ns"

# **One Secret for every pod**, because one StatefulSet is one PodSpec. Each pod stages only the
# certificate `--node` names, so no daemon presents anybody else's - but every pod could read
# every key. That is the trade one workload makes; 20-nodes.yaml says what to do if it is the
# wrong way round for you.
set -- --from-file=users="$out/users" --from-file=peer-ca.pem="$out/peer-ca.pem"
# The revocation list travels with the CA. `certs.sh` writes an empty one at bootstrap, so
# revocation is configured before anybody needs it: turning it on later means restarting every
# node, and the moment a key leaks is the worst moment to be planning a rollout.
[ -f "$out/peer-ca.crl" ] && set -- "$@" --from-file=peer-ca.crl="$out/peer-ca.crl"
for node in $nodes; do
    set -- "$@" --from-file="$node.pem=$out/$node.pem" --from-file="$node.key=$out/$node.key"
done
kubectl -n "$ns" create secret generic big-nodes "$@" \
    --dry-run=client -o yaml | kubectl -n "$ns" apply -f -

# The CA alone. The proxy verifies node certificates and presents none - trusting a CA is not
# being trusted by it - and it authenticates nobody, so there is no users file here either.
kubectl -n "$ns" create secret generic big-proxy \
    --from-file=peer-ca.pem="$out/peer-ca.pem" \
    --dry-run=client -o yaml | kubectl -n "$ns" apply -f -

# What a joining pod registers with. Optional in the manifest: without it a new pod still starts
# and prints the one `add-node` command an operator then runs by hand.
kubectl -n "$ns" create secret generic big-joiner \
    --from-file=credentials="$out/join-credentials" \
    --dry-run=client -o yaml | kubectl -n "$ns" apply -f -

echo
echo "namespace $ns now holds:"
echo "  big-nodes    users, peer-ca.pem, peer-ca.crl, and a certificate for each of: $nodes"
echo "  big-proxy    peer-ca.pem"
echo "  big-joiner   the credential a new pod registers itself with"
echo
echo "Once the cluster is up, make the two roles those credentials name:"
echo
echo "  bigctl --user ops sql \"CREATE ROLE joiner\""
echo "  bigctl --user ops sql \"GRANT OPERATE ON *.* TO joiner\"     # scaling out needs this"
echo "  bigctl --user ops sql \"CREATE ROLE dashboard\""
echo "  bigctl --user ops sql \"GRANT OPERATE ON *.* TO dashboard\"  # /metrics, /verify"
echo
echo "Until then \`kubectl scale\` starts a pod that cannot register itself, and says so in its"
echo "init container's log."
