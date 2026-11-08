#!/usr/bin/env bash
# Regenerate the loopback-fixture TLS material for pf-app-fetch's tests (tsp-ziac.4.3.5).
# TEST-ONLY: the CA reaches the binary solely through the debug-build PF_APP_FETCH_TEST_CA
# hook; release builds compile the hooks out. The keys are deliberately public.
set -euo pipefail
cd "$(dirname "$0")"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
openssl ecparam -name prime256v1 -genkey -noout -out "$tmp/ca.key"
openssl req -x509 -new -key "$tmp/ca.key" -sha256 -days 36500 -subj "/CN=pf-app-fetch test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -outform DER -out ca.der
openssl ecparam -name prime256v1 -genkey -noout -out "$tmp/leaf.key"
openssl pkcs8 -topk8 -nocrypt -in "$tmp/leaf.key" -outform DER -out leaf.key.der
openssl req -new -key "$tmp/leaf.key" -subj "/CN=fixture.test" -out "$tmp/leaf.csr"
printf '%s\n' "basicConstraints=critical,CA:FALSE" "keyUsage=critical,digitalSignature" \
  "extendedKeyUsage=serverAuth" "subjectAltName=DNS:fixture.test,DNS:cdn.fixture.test,DNS:off.fixture.test" \
  > "$tmp/leaf.ext"
openssl x509 -req -in "$tmp/leaf.csr" -CA ca.der -CAform DER -CAkey "$tmp/ca.key" -CAcreateserial \
  -CAserial "$tmp/ca.srl" -days 36500 -sha256 -extfile "$tmp/leaf.ext" -outform DER -out leaf.der
