#!/bin/sh
# The arena on DigitalOcean: make the fleet's secrets (once), create
# everything with Terraform, wait for the nodes, and deploy the game.
#   ./up.sh   (everything it needs is in terraform.tfvars)
set -e
cd "$(dirname "$0")"
[ -f terraform.tfvars ] || { echo "copy terraform.tfvars.example to terraform.tfvars and fill it in"; exit 1; }
BIN=../../../spike/target/release/combd
[ -x $BIN ] || (cd ../../../spike && cargo build --release)
# The fleet's secrets, made here and kept here (gitignored): the wire's CA and
# a certificate per node, a deploy token (the nodes get only its hash), and
# the master key for secrets. Delete .secrets to start over.
S=.secrets
if [ ! -d $S ]; then
    mkdir -p $S && chmod 700 $S && umask 077
    $BIN wire-certs $S/certs 3 >/dev/null
    OUT=$($BIN admin-token 'acme/*:*')
    echo "$OUT" | sed -n 's/.*: \(combd_[0-9a-f]*\)$/\1/p' > $S/token
    echo "$OUT" | sed -n 's/^line for the tokens file: *//p' > $S/tokens
    $BIN master-key $S/master.key >/dev/null
fi
terraform init -input=false >/dev/null
terraform apply
HOST=$(terraform output -raw host)
# Up: each node answers its own name (DNS and cloud-init can take a few minutes).
for n in 0 1 2; do
    printf "waiting for n$n.$HOST"
    i=0; until curl -sf -m 3 -o /dev/null "https://n$n.$HOST/_combd/ping"; do i=$((i + 1)); [ $i -gt 120 ] && { echo " no answer after 10 minutes"; exit 1; }; printf .; sleep 5; done
    echo " up"
done
./deploy.sh
echo "up: https://$HOST (the nearest node), or https://n0.$HOST, n1, n2"
