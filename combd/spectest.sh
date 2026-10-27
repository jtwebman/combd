#!/bin/sh
# Run the official WebAssembly spec testsuite against our interpreter. It
# fetches the suite at a pinned commit (into .testsuite, ignored by git),
# then runs every .wast script; any failure fails the run. Modules that use
# something we don't support are refused at load, and the report counts them
# by reason.
set -e
cd "$(dirname "$0")"
COMMIT=b464a4cd100d98175ae6e3890db89a2e6c8302f7
if [ "$(git -C .testsuite rev-parse HEAD 2>/dev/null)" != "$COMMIT" ]; then
    rm -rf .testsuite
    git init -q .testsuite
    git -C .testsuite fetch -q --depth 1 https://github.com/WebAssembly/testsuite "$COMMIT"
    git -C .testsuite checkout -q FETCH_HEAD
fi
COMBD_WASM_TESTSUITE=.testsuite cargo test --release wasm_spec_testsuite -- --nocapture 2>&1 | sed -nE '/^FAIL|modules refused/,/spec testsuite/p'
