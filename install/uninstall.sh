#!/bin/sh
# uninstall.sh - remove what install.sh put on this machine.
#
# A thin wrapper so there is one obvious thing to run; the work (and the
# flags: --prefix, --purge, --dry-run) is all in install.sh --uninstall.
#
#   sudo ./uninstall.sh            remove binaries, units, driver sources
#   sudo ./uninstall.sh --purge    also delete /etc/pyren and ~/.config/pyren
#
# The 'pyren' group is left alone - removing a group users are still in is
# not this script's call, and an empty group costs nothing.

set -eu

SELF=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

case "${1:-}" in
-h | --help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
esac

exec "$SELF/install.sh" --uninstall "$@"
