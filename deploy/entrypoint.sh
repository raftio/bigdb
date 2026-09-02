#!/bin/sh
# Stages the credentials somewhere their mode is real, drops privileges, and becomes the daemon.
#
# **`serve` is added here, not in the image's CMD.** It is the one word that turns the argument
# list every compose file already writes into the subcommand `big` now needs, and putting it in
# one place means a compose file that predates the rename still starts.
#
# **Why this starts as root.** Two things are true at once: `big serve` refuses a token file that
# anyone but its owner can read, and a bind-mounted file arrives with whatever ownership and
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

stage /etc/big/secrets/tokens /run/big/tokens
stage /etc/big/secrets/peer.token /run/big/peer.token

if [ "$me" = 0 ]; then
    # `setpriv` rather than `su`: no shell in between, no session, no signal indirection - the
    # daemon becomes PID 1's child directly and sees a stop signal as itself.
    # The `big` in `--reuid=big` is the user; the `big` after `--init-groups` is the binary.
    # They are spelled the same and are not the same thing.
    exec setpriv --reuid=big --regid=big --init-groups big serve "$@"
fi
exec big serve "$@"
