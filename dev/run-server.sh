#!/bin/sh
# Inside the dev box: (re)start the server from the static debug build.
pkill -f "^/usr/local/bin/tugport server" 2>/dev/null; sleep 1
BIN=/target/$(uname -m)-unknown-linux-musl/debug/tugport
cp "$BIN" /usr/local/bin/tugport
RUST_LOG=${RUST_LOG:-tugport=debug} setsid nohup /usr/local/bin/tugport server </dev/null >/var/log/tugport.log 2>&1 &
for i in $(seq 1 50); do curl -s localhost:8484/api/setup >/dev/null && break; sleep 0.2; done
grep -o 'http://[^ ]*/setup#[A-Za-z0-9]*' /var/log/tugport.log | tail -1
