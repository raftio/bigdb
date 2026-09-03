#!/bin/sh
# Stages the credentials somewhere their mode is real, drops privileges, and becomes the daemon.
#
# **`serve` is added here, not in the image's CMD.** It is the one word that turns the argument
# list every compose file already writes into the subcommand `big` now needs, and putting it in
# one place means a compose file that predates the rename still starts.
#
# **Why this starts as root.** Two things are true at once: `big serve` refuses a users file or a
# private key that anyone but its owner can read, and a bind-mounted file arrives with whatever ownership and
# mode the *host* gave it - root-owned `600` on one machine, `0755` on Docker Desktop, uid 1000
# somewhere else. Nothing the image can do makes those two agree. Reading the file as root and
# writing a private copy makes them agree, and costs the container root for the length of one
# `install`.
#
# **The daemon never runs as root.** The last line drops to `big` and stays there. If this is
# already running unprivileged - `user:` in a compose file, a hardened runtime - the staging is
# done with whatever access it has and the drop is skipped, so the strict deployment still
# works as long as the host files are readable by that user.
#
# What is mounted is the *directory*, never the files. A single-file bind mount breaks the
# first time anything replaces the file rather than writing through it - which is what every
# editor and most secret managers do - and the container goes on holding the inode that used to
# be there.
set -eu

me=$(id -u)

stage() {
    [ -r "$1" ] || return 0
    if [ "$me" = 0 ]; then
        # Owner and mode set as it copies, so there is no moment where the file exists and is
        # readable by anyone else.
        install -o big -g big -m 600 "$1" "$2"
    else
        install -m 600 "$1" "$2"
    fi
}

stage /etc/big/secrets/users /run/big/users
# The CA is not a secret - it is what everybody checks against - but it is staged the same way so
# that one rule covers the directory. The per-node key is a secret, and gets the same mode 600.
stage /etc/big/secrets/peer-ca.pem /run/big/peer-ca.pem

# Which node this is, read out of the arguments rather than out of a second environment variable.
# The name is already in the command line as `--node`, and a deployment that had to write it
# twice is a deployment where the two can disagree - which would present as a node holding a
# certificate for somebody else and being refused by every peer.
node=""
want=""
for arg do
    if [ -n "$want" ]; then
        node=$arg
        want=""
    elif [ "$arg" = "--node" ]; then
        want=1
    fi
done

if [ -n "$node" ]; then
    stage "/etc/big/secrets/$node.pem" /run/big/peer.pem
    stage "/etc/big/secrets/$node.key" /run/big/peer.key
fi

if [ "$me" = 0 ]; then
    # `setpriv` rather than `su`: no shell in between, no session, no signal indirection - the
    # daemon becomes PID 1's child directly and sees a stop signal as itself.
    # The `big` in `--reuid=big` is the user; the `big` after `--init-groups` is the binary.
    # They are spelled the same and are not the same thing.
    exec setpriv --reuid=big --regid=big --init-groups big serve "$@"
fi
exec big serve "$@"
