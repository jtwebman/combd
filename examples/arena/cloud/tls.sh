#!/bin/sh
# Give every running node the current certificate (cloud-init only sets it
# up on a new droplet). combd sees the new file within seconds and serves it
# to new connections; open ones keep theirs. Run it after `terraform apply`
# changes the certificate (a new app's name, or a renewal).
set -e
cd "$(dirname "$0")"
HOST=$(terraform output -raw host)
T=$(mktemp -d) && trap 'rm -rf "$T"' EXIT && chmod 700 "$T"
terraform show -json | python3 -c '
import json, sys
c = next(r for r in json.load(sys.stdin)["values"]["root_module"]["resources"] if r["address"] == "acme_certificate.arena")["values"]
open(sys.argv[1] + "/cert.pem", "w").write(c["certificate_pem"] + c["issuer_pem"])
open(sys.argv[1] + "/key.pem", "w").write(c["private_key_pem"])
' "$T"
for n in 0 1 2; do
    echo "n$n.$HOST: certificate"
    ssh -o ConnectTimeout=10 "root@n$n.$HOST" "mkdir -p /etc/combd/tls"
    scp -q -o ConnectTimeout=10 "$T/key.pem" "root@n$n.$HOST:/etc/combd/tls/key.pem.new"
    scp -q -o ConnectTimeout=10 "$T/cert.pem" "root@n$n.$HOST:/etc/combd/tls/cert.pem.new"
    # combd reloads when the certificate's file changes: the key goes first.
    ssh -o ConnectTimeout=10 "root@n$n.$HOST" "chgrp combd /etc/combd/tls/key.pem.new && chmod 640 /etc/combd/tls/key.pem.new && mv /etc/combd/tls/key.pem.new /etc/combd/tls/key.pem && mv /etc/combd/tls/cert.pem.new /etc/combd/tls/cert.pem"
done
