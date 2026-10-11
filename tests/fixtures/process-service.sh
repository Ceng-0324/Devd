#!/bin/sh

case "$1" in
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
