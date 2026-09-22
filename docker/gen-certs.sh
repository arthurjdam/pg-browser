#!/usr/bin/env bash
# Generates a throwaway CA + server cert (SAN: localhost, 127.0.0.1) and a client cert
# for the pg17-ssl test server. Output goes to docker/certs (gitignored).
set -euo pipefail
cd "$(dirname "$0")/certs"
openssl req -new -x509 -days 3650 -nodes -newkey rsa:2048 -subj "/CN=pgb-test-ca" -keyout ca.key -out ca.crt
openssl req -new -nodes -newkey rsa:2048 -subj "/CN=localhost" -keyout server.key -out server.csr
printf "subjectAltName=DNS:localhost,IP:127.0.0.1" > san.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 3650 -extfile san.ext -out server.crt
openssl req -new -nodes -newkey rsa:2048 -subj "/CN=pgb_admin" -keyout client.key -out client.csr
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 3650 -out client.crt
# postgres inside the container requires the key to be owned by uid 999 with 0600; docker bind mounts
# keep host ownership, so relax it for local testing.
chmod 644 server.key
rm -f *.csr san.ext
echo "certs written to $(pwd)"
