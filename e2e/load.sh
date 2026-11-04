#!/usr/bin/env bash
# The load run of section 13: the registrar in a container limited to two CPUs, the fakes in
# place of the outside services, and mtpool-loadgen with simulated nodes and viewers.
#
#   e2e/load.sh [mtpool-loadgen arguments]      e.g. e2e/load.sh --nodes 1000 --duration 300
#
# The generator prints the JSON report on its standard output; it is kept in
# tmp/load/report.json. The stand is removed at the end.
set -euo pipefail

cd "$(dirname "$0")/.."
mkdir -p tmp/load
rm -f tmp/load/report.json

project="mtpool-load-$$"
compose=(docker compose -p "$project" -f e2e/compose.yml -f e2e/compose.load.yml)
trap '"${compose[@]}" --profile load down -v --remove-orphans >/dev/null 2>&1 || true' EXIT

"${compose[@]}" --profile load build loadgen
"${compose[@]}" up -d --build --wait fakes registrar

limit=$(docker inspect --format '{{.HostConfig.NanoCpus}}' "$("${compose[@]}" ps -q registrar)")
echo "registrar cpu limit (NanoCpus): $limit" >&2

MTPOOL_ADMIN_TOKEN=$("${compose[@]}" exec -T registrar mtpool-registrar admin reset-token)
export MTPOOL_ADMIN_TOKEN

"${compose[@]}" run --rm -T loadgen \
  --registrar http://127.0.0.1:8080 \
  --registrar-pid 1 \
  "$@" > tmp/load/report.json

echo "report: tmp/load/report.json" >&2
