#!/bin/sh
set -eu
case "$1" in
  clients) cat "$HYPRLOOM_REPORT_FIXTURE/clients.json" ;;
  monitors) printf '%s\n' '[{"id":0,"name":"DP-1","width":1920,"height":1080,"x":0,"y":0,"transform":0}]' ;;
  version) printf '%s\n' 'Hyprland fixture' ;;
  --batch|dispatch)
    printf '%s\n' "$*" >> "$HYPRLOOM_REPORT_FIXTURE/dispatches"
    if [ "${HYPRLOOM_REPORT_FAIL_DISPATCH:-0}" = 1 ]; then
      printf '%s\n' 'fixture dispatch refused' >&2
      exit 1
    fi
    ;;
  *) printf 'unexpected fixture command: %s\n' "$*" >&2; exit 1 ;;
esac
