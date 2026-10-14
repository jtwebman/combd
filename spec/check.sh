#!/bin/sh
# Model-check the ownership spec. The correct protocol must pass, and every
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
tlc() { java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto -deadlock -metadir ".tools/states/$1" -config "$1.cfg" MCOwnership.tla 2>&1; }
out=$(tlc MCOwnership)
if echo "$out" | grep -q "No error has been found"; then
    echo "ok    MCOwnership: $(echo "$out" | grep -o '[0-9]* distinct states found, 0 states left' | cut -d' ' -f1) states, no errors"
else
    echo "FAIL  MCOwnership"; echo "$out" | tail -40; exit 1
fi
for bug in BugSkipProof BugLoadBeforeCas BugNoManifest BugReuseEpoch; do
    v=$(tlc "$bug" | grep -o 'Invariant [A-Za-z]* is violated' || true)
    if [ -n "$v" ]; then echo "ok    $bug: $v (as it must be)"; else echo "FAIL  $bug passed: the checker can't see this bug"; exit 1; fi
done
