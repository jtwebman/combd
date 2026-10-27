# Autobahn: the WebSocket conformance suite, against combd

An echo `ws` process (`echo/`: each message sent back as it came) and the
Autobahn TestSuite's client, plain and over TLS:

```sh
(cd echo && cargo build --release --target wasm32-unknown-unknown)
mkdir -p app/modules && cp echo/target/wasm32-unknown-unknown/release/combd_echo.wasm app/modules/echo.wasm
../../target/release/combd serve-app interp app 9001 &
COMBD_TLS_CERT=../edge/certs/cert.pem COMBD_TLS_KEY=../edge/certs/key.pem \
  ../../target/release/combd serve-app interp app 9443 &   # certs: ../edge/run.sh makes them
mkdir -p reports/all
docker run --rm -v "$PWD/config:/config" -v "$PWD/reports:/reports" \
  crossbario/autobahn-testsuite wstest -m fuzzingclient -s /config/all.json
```

`reports/all/index.html` has every case. `fuzzingclient.json` leaves out
9.* (messages up to 16 MiB: over `COMBD_MAX_WS_MESSAGE`, refused with 1009
by design), 12.* and 13.* (compression, not offered). Results: `RESULTS.md`,
"Single mode".
