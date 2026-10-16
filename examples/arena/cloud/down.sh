#!/bin/sh
# Take it all down. The R2 bucket isn't Terraform's: it stays until you
# empty and delete it (wrangler r2 bucket delete <name>, once it's empty).
set -e
cd "$(dirname "$0")"
terraform destroy
