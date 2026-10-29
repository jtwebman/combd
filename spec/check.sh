#!/bin/sh
# Model-check the specs (the catalog, D2 ownership, D3 replication). The correct protocol must pass, and every
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
tlc() { java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto -deadlock -metadir ".tools/states/$2" -config "$2.cfg" "$1.tla" 2>&1 || true; }
# pass MODULE CFG: the correct protocol must check clean.
pass() {
    out=$(tlc "$1" "$2")
    if echo "$out" | grep -q "No error has been found"; then
        echo "ok    $2: $(echo "$out" | grep -o '[0-9]* distinct states found, 0 states left' | cut -d' ' -f1) states, no errors"
    else
        echo "FAIL  $2"; echo "$out" | tail -40; exit 1
    fi
}
# sim MODULE CFG SECONDS: a model too big to finish here, walked at random
# for that long; it must break nothing. (perl: macOS has no timeout.)
sim() {
    out=$(perl -e 'alarm shift; exec @ARGV' "$3" java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -simulate -depth 80 -workers auto -deadlock -metadir ".tools/states/$2" -config "$2.cfg" "$1.tla" 2>&1 || true)
    rm -rf ".tools/states/$2"
    if echo "$out" | grep -q '^Starting' && ! echo "$out" | grep -q 'Error'; then
        echo "ok    $2: no errors in $3 s of random runs"
    else
        echo "FAIL  $2"; echo "$out" | tail -40; exit 1
    fi
}
# fail MODULE CFG...: each known-bad version must break an invariant.
fail() {
    m=$1; shift
    for bug in "$@"; do
        v=$(tlc "$m" "$bug" | grep -o 'Invariant [A-Za-z]* is violated' || true)
        if [ -n "$v" ]; then echo "ok    $bug: $v (as it must be)"; else echo "FAIL  $bug passed: the checker can't see this bug"; exit 1; fi
    done
}
pass MCCatalog MCCatalog
fail MCCatalog CatAckBeforeWrite CatBlindSlot CatApplyAnyOrder CatUncoveredCheck CatTrimNoGate CatTrimEarly CatReadNoGate
pass MCOwnership MCOwnership
fail MCOwnership BugSkipProof BugLoadBeforeCas BugNoManifest BugReuseEpoch
pass MCReplication MCReplication
fail MCReplication RepAckOne RepNoFence RepLoadBeforeFence RepTruncEarly RepSealEarly RepFenceIfManifest RepAckNoProof RepLateManifest RepLeaseNoPromise RepLeaseOvertrust RepNoTail
# With a peer's disk lost once, and one write (400 million states, about an
# hour and a half). With two writes it's too big to finish on a laptop
# (stopped clean at 1.1 billion states), so that one is walked at
# random, 10 minutes by default (WIPE_SECS).
pass MCReplication RepWipe
sim MCReplication RepWipe2 "${WIPE_SECS:-600}"
fail MCReplication RepTrustWiped RepWipeForgetsFence
