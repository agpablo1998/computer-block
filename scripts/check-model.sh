#!/usr/bin/env bash
# Model-check the TLA+ spec and replay its state graph against the Rust core.
#
#   scripts/check-model.sh          fast: one window (seconds)
#   scripts/check-model.sh --full   all properties incl. liveness, two windows,
#                                   and conformance on the two-window graph
set -euo pipefail
cd "$(dirname "$0")/.."

TLA_VERSION=1.7.4
TLA_SHA256=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
ROOT=$PWD
JAR=$ROOT/target/tla/tla2tools-$TLA_VERSION.jar

mkdir -p target/tla
if [ ! -f "$JAR" ]; then
  curl -fsSL -o "$JAR.tmp" "https://github.com/tlaplus/tlaplus/releases/download/v$TLA_VERSION/tla2tools.jar"
  echo "$TLA_SHA256  $JAR.tmp" | sha256sum -c -
  mv "$JAR.tmp" "$JAR"
fi

tlc() {
  java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto -cleanup -metadir "$ROOT/target/tla/meta" "$@"
}

graph=$ROOT/target/tla/state-graph.dot
if [ "${1:-}" = "--full" ]; then
  echo "== Safety + liveness, two windows (spec/MC.cfg)"
  (cd spec && tlc -config MC.cfg Lockdown.tla)
  echo "== State graph, two windows"
  sed 's/Win = {w1}/Win = {w1, w2}/' spec/Conformance.cfg > target/tla/Conformance2.cfg
  cp spec/Lockdown.tla target/tla/
  (cd target/tla && tlc -config Conformance2.cfg -dump dot,actionlabels state-graph.dot Lockdown.tla)
else
  echo "== State graph, one window (spec/Conformance.cfg)"
  (cd spec && tlc -config Conformance.cfg -dump dot,actionlabels "$graph" Lockdown.tla)
fi

echo "== Rust core vs. model"
LOCKDOWN_STATE_GRAPH="$graph" LOCKDOWN_REQUIRE_GRAPH=1 \
  cargo test --release --lib conformance -- --nocapture
