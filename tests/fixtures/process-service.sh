#!/bin/sh

case "$1" in
    graceful)
        trap 'printf "stopped\n"; exit 0' TERM
        sleep 60 &
        printf 'ready\n'
        # A foreground sleep can defer the trap; wait is signal-interruptible.
        # A group signal may wake wait when the descendant exits before this
        # shell receives TERM. Keep the leader alive for its own trap without
        # forking another child in the middle of group shutdown.
        while :; do
            wait || :
        done
        ;;
    stubborn)
        trap '' TERM
        sleep 60 &
        printf 'ready\n'
        wait
        ;;
    tree)
        trap 'exit 0' TERM
        /bin/sh "$0" stubborn &
        printf 'parent-ready\n'
        wait
        ;;
    background)
        /bin/sh "$0" stubborn &
        printf 'leader-exiting\n'
        exit 7
        ;;
    output)
        i=0
        while [ "$i" -lt 4096 ]; do
            printf '0123456789abcdefghijklmnopqrstuv\n'
            printf '0123456789abcdefghijklmnopqrstuv\n' >&2
            i=$((i + 1))
        done
        ;;
    args)
        shift
        printf '<%s>\n' "$@"
        ;;
    *)
        exit 2
        ;;
esac
