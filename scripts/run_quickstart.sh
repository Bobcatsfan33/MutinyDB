#!/usr/bin/env bash
# The verbatim-quickstart discipline (FlockDB's, adopted at M6): CI runs EXACTLY the commands the
# README shows a stranger, extracted from between the quickstart markers — so a silently broken
# step turns the job red instead of rotting in prose. Tooth (c) of the M6 gate.
set -euo pipefail
cd "$(dirname "$0")/.."

awk '/<!-- quickstart:begin -->/{flag=1; next} /<!-- quickstart:end -->/{flag=0} flag' README.md \
  | sed -n '/^```bash$/,/^```$/p' | sed '1d;$d' > /tmp/mutiny-quickstart.sh

if [ ! -s /tmp/mutiny-quickstart.sh ]; then
  echo "the README quickstart block is missing or empty" >&2
  exit 1
fi
echo "── running the README quickstart verbatim ──"
cat /tmp/mutiny-quickstart.sh
bash -euo pipefail /tmp/mutiny-quickstart.sh
echo "── quickstart green ──"
