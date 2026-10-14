#!/bin/sh
# Model-check the specs (D2 ownership, D3 replication). The correct protocol must pass, and every
# known-bad version must fail; if a bad one passes, the checker is blind.
set -e
cd "$(dirname "$0")"
JAR=.tools/tla2tools.jar
SHA=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
if [ ! -f "$JAR" ]; then
    mkdir -p .tools
    curl -sSL -o "$JAR" https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar
fi
echo "$SHA  $JAR" | shasum -a 256 -c - >/dev/null
tlc() { java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto -deadlock -metadir ".tools/states/$2" -config "$2.cfg" "$1.tla" 2>&1; }
check() {
    model=$1; shift
    out=$(tlc "$model" "$model" || true)
    if echo "$out" | grep -q "No error has been found"; then
        echo "ok    $model: $(echo "$out" | grep -o '[0-9]* distinct states found, 0 states left' | cut -d' ' -f1) states, no errors"
    else
        echo "FAIL  $model"; echo "$out" | tail -40; exit 1
    fi
    for bug in "$@"; do
        v=$(tlc "$model" "$bug" | grep -o 'Invariant [A-Za-z]* is violated' || true)
        if [ -n "$v" ]; then echo "ok    $bug: $v (as it must be)"; else echo "FAIL  $bug passed: the checker can't see this bug"; exit 1; fi
    done
}
check MCOwnership BugSkipProof BugLoadBeforeCas BugNoManifest BugReuseEpoch
check MCReplication RepAckOne RepNoFence RepLoadBeforeFence RepTruncEarly RepSealEarly RepFenceIfManifest RepAckNoProof
