#!/bin/sh
# Stages the TLS key somewhere its mode is real, drops privileges, and becomes the proxy.
#
# A trimmed copy of `deploy/entrypoint.sh`, and the trimming is the interesting part. That one
# scans the cluster file to work out which node this container is; this one does not, because a
# proxy is not a node. It has no `--node`, no database file and no lock to take.
#
# **Why it starts as root.** `--tls-key` is held to the same rule `big serve` holds its own key
# to: a private key anyone can read is not a private key. A bind-mounted file arrives with
# whatever ownership the *host* gave it, and nothing the image can do makes those two agree.
# Reading it as root and writing a private copy does, and costs root for the length of one
# `install`.
#
# **The proxy never runs as root.** The last line drops to `bigproxy` and stays there. If this
# is already running unprivileged, the staging is done with whatever access it has and the drop
# is skipped.
#
# What is mounted is the *directory*, never the files: a single-file bind mount breaks the first
# time anything replaces the file rather than writing through it, which is what every editor and
# most secret managers do.
set -eu

me=$(id -u)

stage() {
    [ -r "$1" ] || return 0
    if [ "$me" = 0 ]; then
        install -o bigproxy -g bigproxy -m "$2" "$1" "/run/bigproxy/$(basename "$1")"
    else
        install -m "$2" "$1" "/run/bigproxy/$(basename "$1")"
    fi
}

# 600 for the key, 644 for everything else. A CA certificate is public — it is what a
# certificate chains *to*, not a secret — and giving it 600 would suggest otherwise.
stage /etc/bigproxy/secrets/tls-key.pem 600
stage /etc/bigproxy/secrets/tls-cert.pem 644
stage /etc/bigproxy/secrets/peer-ca.pem 644

if [ "$me" = 0 ]; then
    exec setpriv --reuid=bigproxy --regid=bigproxy --init-groups \
        /usr/local/bin/bigproxy "$@"
fi
exec /usr/local/bin/bigproxy "$@"
