#!/usr/bin/env bash
# Stands in for the console's Managers screen, which isn't built yet: it
# writes the inbox and reads the outbox exactly as Drupal would, as www-data in
# the app container. The console can't pair (E-02): a new Manager asks the
# engine itself and the owner's primary approves it.
#
#   engine/console-standin.sh <project> list          # pairings, as the console sees them
#   engine/console-standin.sh <project> revoke <id>
set -euo pipefail
P=${1:?compose project, e.g. atelier-engine-test}
CMD=${2:?list | revoke <id>}
IN=/opt/drupal/engine-inbox
OUT=/opt/drupal/engine-outbox
app() { docker exec -u www-data "$P-app-1" "$@"; }

case "$CMD" in
  list) app cat "$OUT/pairings.json"; echo ;;
  revoke) app sh -c "printf '{\"id\": \"%s\"}' '${3:?id}' > $IN/revoke.request"; sleep 2; app cat "$OUT/pairings.json"; echo ;;
  *) echo "unknown command $CMD" >&2; exit 2 ;;
esac
