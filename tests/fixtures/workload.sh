#!/bin/sh
# Real supervised workload. All coordination files live in the test project.
set -eu
name=$1
sleep 600 &
descendant=$!
stop() {
    trap '' TERM INT
    kill "$descendant" 2>/dev/null || :
    wait "$descendant" 2>/dev/null || :
    printf 'stop %s %s\n' "$name" "$$" >> events
    printf '%s stopped\n' "$name"
    exit 0
}
trap stop TERM INT
printf '%s\n' "$descendant" > "$name.$$.pid.tmp"
mv "$name.$$.pid.tmp" "$name.$$.pid"
printf 'start %s %s\n' "$name" "$$" >> events
printf '%s stdout-ready\n' "$name"
printf '%s stderr-ready\n' "$name" >&2
while :; do
    if [ -f "crash-$name" ]; then
        rm "crash-$name"
        printf '%s injected crash, exit 23\n' "$name" >&2
        # Leave a descendant for the supervisor to clean after leader exit.
        exit 23
    fi
    sleep 0.02 &
    wait $! || :
done
