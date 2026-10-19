#!/bin/sh
# Give every running node the current certificate and Caddyfile (cloud-init
# only sets them up on a new droplet), and reload Caddy: no dropped
# connections. Run it after `terraform apply` changes the certificate (a new
# app's name, or a renewal).
set -e
cd "$(dirname "$0")"
HOST=$(terraform output -raw host)
SITES=$(terraform output -raw sites)
T=$(mktemp -d) && trap 'rm -rf "$T"' EXIT && chmod 700 "$T"
terraform show -json | python3 -c '
import json, sys
c = next(r for r in json.load(sys.stdin)["values"]["root_module"]["resources"] if r["address"] == "acme_certificate.arena")["values"]
open(sys.argv[1] + "/cert.pem", "w").write(c["certificate_pem"] + c["issuer_pem"])
open(sys.argv[1] + "/key.pem", "w").write(c["private_key_pem"])
' "$T"
# The same as cloud-init.tftpl's.
cat > "$T/Caddyfile" <<CADDY
$SITES {
  tls /etc/caddy/cert.pem /etc/caddy/key.pem
  # While combd restarts (a new image), hold requests and retry
  # instead of answering 502.
  reverse_proxy 127.0.0.1:8080 {
    lb_try_duration 20s
    lb_try_interval 250ms
  }
}
CADDY
for n in 0 1 2; do
    echo "n$n.$HOST: certificate and Caddyfile"
    scp -q -o ConnectTimeout=10 "$T/cert.pem" "$T/key.pem" "$T/Caddyfile" "root@n$n.$HOST:/etc/caddy/"
    ssh -o ConnectTimeout=10 "root@n$n.$HOST" "chmod 600 /etc/caddy/key.pem && docker exec caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile"
done
