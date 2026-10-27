#!/bin/sh
# Build the game and deploy it through node 0's admin API (TLS, the fleet's
# CA). Run it again after a change: a live deploy, every node in a moment.
set -e
cd "$(dirname "$0")"
BIN=../../../combd/target/release/combd
HOST=$(terraform output -raw host)
../build.sh
COMBD_TOKEN=$(cat .secrets/token) COMBD_ADMIN_CA=.secrets/certs/ca.pem COMBD_ADMIN_NAME=node0.combd \
    $BIN deploy ../app acme "https://n0.$HOST:9443"
