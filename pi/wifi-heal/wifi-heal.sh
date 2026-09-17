#!/usr/bin/env bash
# wifi-heal.sh - self-heal a Wi-Fi link that came back degraded.
#
# After a hard power cut the Pi's radio has come up with ~25ms average
# latency and heavy jitter to the gateway while signal, bitrate, and
# powersave all looked fine; bouncing the radio fixed it. This script
# measures gateway latency, compares against a learned healthy baseline,
# and bounces the radio with bounded retries. Run by systemd on every
# wlan0 connect (NetworkManager dispatcher) and on a periodic timer.
#
# Tunables via /etc/default/beaglewing-wifi-heal (all optional).

set -u

# Single instance: the dispatcher fires on every connect, including the
# reconnect a bounce causes, so overlapping runs would stack bounces.
exec 9>/run/beaglewing-wifi-heal.lock
flock -n 9 || { echo "[wifi-heal] another run in progress; skipping"; exit 0; }

IFACE=${IFACE:-wlan0}
# What to ping. Prefer the machine that actually talks to this Pi (the
# controlling desktop): gateways often rate-limit or deprioritize ICMP
# and read as jittery even when the real path is fine. Falls back to the
# default gateway when TARGET is unset or unreachable.
TARGET=${TARGET:-}
DEGRADED_MS=${DEGRADED_MS:-15}     # absolute floor: never act below this
FACTOR=${FACTOR:-3}                # ...and only if worse than FACTOR x baseline
ATTEMPTS=${ATTEMPTS:-4}
RETRY_WAIT=${RETRY_WAIT:-120}      # seconds between attempts
SETTLE=${SETTLE:-25}               # seconds to let the radio reassociate
INITIAL_SETTLE=${INITIAL_SETTLE:-30}
MAX_BOUNCES_PER_HOUR=${MAX_BOUNCES_PER_HOUR:-3}
STATE=/var/lib/beaglewing
mkdir -p "$STATE"

log() { echo "[wifi-heal] $*"; }

gateway() { ip -4 route show default dev "$IFACE" 2>/dev/null | awk '{print $3; exit}'; }
connected() { nmcli -t -f GENERAL.STATE dev show "$IFACE" 2>/dev/null | grep -q connected; }
measure() {
    local host avg
    for host in $TARGET $(gateway); do
        [[ -n $host ]] || continue
        avg=$(ping -c 20 -i 0.2 -W 1 "$host" 2>/dev/null | awk -F'/' '/rtt/ {print $5}')
        if [[ -n $avg ]]; then echo "$avg"; return 0; fi
    done
    return 1
}
bounces_last_hour() {
    local now; now=$(date +%s)
    [[ -f $STATE/bounces ]] || { echo 0; return; }
    awk -v now="$now" '$1 > now - 3600' "$STATE/bounces" | wc -l
}
bounce() {
    date +%s >> "$STATE/bounces"
    nmcli radio wifi off
    sleep 4
    nmcli radio wifi on
}
is_degraded() {
    # $1 = avg ms, $2 = baseline ms or empty
    awk -v a="$1" -v b="${2:-0}" -v floor="$DEGRADED_MS" -v f="$FACTOR" \
        'BEGIN { if (a <= floor) exit 1; if (b > 0 && a <= f * b) exit 1; exit 0 }'
}
learn() {
    # slow-moving baseline, healthy samples only
    local avg=$1 base=${2:-}
    if [[ -z $base ]]; then echo "$avg"; else awk -v a="$avg" -v b="$base" 'BEGIN { printf "%.2f", (3*b + a) / 4 }'; fi
}

force=0
[[ ${1:-} == --force ]] && force=1
[[ $force -eq 0 ]] && sleep "$INITIAL_SETTLE"

baseline=$(cat "$STATE/wifi-baseline" 2>/dev/null || true)

for attempt in $(seq 1 "$ATTEMPTS"); do
    connected || { log "$IFACE not connected; nothing to do"; exit 0; }
    avg=$(measure) || { log "no gateway reachable; nothing to do"; exit 0; }
    [[ -n $avg ]] || { log "gateway not answering; nothing to do"; exit 0; }

    if [[ $force -eq 0 ]] && ! is_degraded "$avg" "$baseline"; then
        if awk -v a="$avg" -v floor="$DEGRADED_MS" 'BEGIN { exit !(a < floor) }'; then
            learn "$avg" "$baseline" > "$STATE/wifi-baseline"
        fi
        log "healthy: ${avg}ms to gateway (baseline ${baseline:-unset})"
        exit 0
    fi

    if (( $(bounces_last_hour) >= MAX_BOUNCES_PER_HOUR )); then
        log "degraded (${avg}ms) but $MAX_BOUNCES_PER_HOUR bounces in the last hour; giving up"
        exit 0
    fi
    log "degraded: ${avg}ms to gateway (baseline ${baseline:-unset}); bouncing radio, attempt $attempt/$ATTEMPTS"
    bounce
    sleep "$SETTLE"
    force=0
    after=$(measure) || after=""
    if [[ -n $after ]] && ! is_degraded "$after" "$baseline"; then
        log "recovered: ${after}ms after bounce"
        exit 0
    fi
    log "still ${after:-unreachable} after bounce"
    (( attempt < ATTEMPTS )) && sleep "$RETRY_WAIT"
done
log "giving up after $ATTEMPTS attempts"
