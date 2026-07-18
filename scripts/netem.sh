#!/usr/bin/env bash
# Network-impairment recipes for testing jam on localhost (Linux only).
# Usage: scripts/netem.sh start|stop|status [interface]
#
#   start: 20 ms delay ±5 ms normal-distributed jitter, 2% loss, 1% reorder
#   stop:  remove the qdisc
#
# Run a host and client on the impaired interface (default: lo) and watch
# the jitter buffer adapt in the status display.
set -euo pipefail

IFACE="${2:-lo}"

case "${1:-}" in
  start)
    sudo tc qdisc replace dev "$IFACE" root netem \
      delay 20ms 5ms distribution normal loss 2% reorder 1%
    echo "netem active on $IFACE: 20ms ±5ms jitter, 2% loss, 1% reorder"
    ;;
  stop)
    sudo tc qdisc del dev "$IFACE" root 2>/dev/null || true
    echo "netem removed from $IFACE"
    ;;
  status)
    tc qdisc show dev "$IFACE"
    ;;
  *)
    echo "usage: $0 start|stop|status [interface]" >&2
    exit 1
    ;;
esac
