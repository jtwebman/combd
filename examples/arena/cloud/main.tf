# The arena on real nodes around the world: one DigitalOcean droplet per
# region, an R2 bucket, and one name (arena.<domain>) that Cloudflare answers
# with the nearest healthy node, DNS only (not proxied, so a player's ping is
# to the node itself). Each node also gets its own name, n<i>.arena.<domain>.
# HTTPS (a .dev name must have it): a Let's Encrypt certificate for the name
# and *.name, got through a DNS challenge on Cloudflare, served by combd
# itself on each node (single mode: it ends TLS; 80 redirects).
#
#   ./up.sh     secrets, then terraform apply, then deploy the game
#   ./down.sh   terraform destroy (the bucket stays: empty it to delete it)
#
# Everything, tokens too, comes from terraform.tfvars (gitignored; see
# terraform.tfvars.example). Provider settings never go in the state.

terraform {
  required_version = ">= 1.5"
  required_providers {
    digitalocean = { source = "digitalocean/digitalocean", version = "~> 2.40" }
    cloudflare   = { source = "cloudflare/cloudflare", version = "~> 4.52" }
    acme         = { source = "vancluever/acme", version = "~> 2.26" }
    tls          = { source = "hashicorp/tls", version = "~> 4.0" }
  }
}

provider "digitalocean" {
  token = var.do_token
}
provider "cloudflare" {
  api_token = var.cloudflare_api_token
}
provider "acme" {
  server_url = "https://acme-v02.api.letsencrypt.org/directory"
}

variable "domain" {
  description = "A zone on Cloudflare, e.g. example.com"
}
variable "name" {
  description = "The demo's name under the domain: arena.example.com"
  default     = "arena"
}
variable "regions" {
  description = "One node per region, node 0 first"
  default     = ["nyc3", "fra1", "sgp1"]
}
variable "size" {
  default = "s-1vcpu-1gb"
}
variable "image" {
  default = "ghcr.io/jtwebman/combd-arena:latest"
}
variable "admin_cidr" {
  description = "Who may reach SSH and the admin API: your IP, as a.b.c.d/32"
}
variable "do_token" {
  description = "DigitalOcean API token (write)"
  sensitive   = true
}
variable "cloudflare_api_token" {
  description = "Cloudflare API token: Zone DNS Edit, Zone Load Balancers Edit, Account Load Balancing Monitors and Pools Edit"
  sensitive   = true
}
variable "cloudflare_account_id" {}
variable "r2_bucket" {
  default = "combd-arena"
}
variable "r2_key" {
  sensitive = true
}
variable "r2_secret" {
  sensitive = true
}
variable "ssh_public_key" {
  default = "~/.ssh/id_ed25519.pub"
}
variable "do_project" {
  default = "Combd"
}
variable "acme_email" {
  description = "Let's Encrypt account email (it may send notices there)"
}
variable "apps" {
  description = "More apps on the same nodes, each at <app>.<domain>: in the certificate, and a name for each (all the nodes, round robin)"
  default     = ["million"]
}
variable "load_balancer" {
  description = "Cloudflare Load Balancing (a paid add-on): the page itself from the nearest node. Off: arena.<domain> is the nodes, round robin; the game still plays on the nearest (it measures)."
  default     = false
}

locals {
  host    = "${var.name}.${var.domain}"
  apps    = [for a in var.apps : "${a}.${var.domain}"]
  secrets = "${path.module}/.secrets"
  nodes   = { for i, r in var.regions : tostring(i) => r }
  peers   = join(",", [for i, r in var.regions : "${i}=n${i}.${local.host}:7100"])
  public  = join(",", [for i, r in var.regions : "${i}=https://n${i}.${local.host}"])
  # Where each region is, for the load balancer's nearest-node steering.
  where = {
    nyc3 = [40.7, -74.0], sfo3 = [37.8, -122.4], tor1 = [43.7, -79.4], lon1 = [51.5, -0.1], ams3 = [52.4, 4.9],
    fra1 = [50.1, 8.7], blr1 = [12.97, 77.6], sgp1 = [1.35, 103.8], syd1 = [-33.9, 151.2],
  }
}

data "cloudflare_zone" "zone" {
  name = var.domain
}

# The certificate: arena.<domain>, *.arena.<domain>, and the other apps'
# names, for every node. Running nodes get a new one from tls.sh.
resource "tls_private_key" "acme" {
  algorithm   = "ECDSA"
  ecdsa_curve = "P256"
}

resource "acme_registration" "me" {
  account_key_pem = tls_private_key.acme.private_key_pem
  email_address   = var.acme_email
}

resource "acme_certificate" "arena" {
  account_key_pem           = acme_registration.me.account_key_pem
  common_name               = local.host
  subject_alternative_names = concat(["*.${local.host}"], local.apps)
  key_type                  = "P256"
  # A replaced certificate isn't revoked: running nodes serve it until
  # tls.sh gives them the new one.
  revoke_certificate_on_destroy = false
  dns_challenge {
    provider = "cloudflare"
    config   = { CF_DNS_API_TOKEN = var.cloudflare_api_token }
  }
}

data "digitalocean_project" "project" {
  name = var.do_project
}

resource "digitalocean_ssh_key" "me" {
  name       = "combd-arena"
  public_key = file(pathexpand(var.ssh_public_key))
}

resource "digitalocean_droplet" "node" {
  for_each = local.nodes
  name     = "combd-arena-${each.key}-${each.value}"
  region   = each.value
  size     = var.size
  image    = "ubuntu-24-04-x64"
  ssh_keys = [digitalocean_ssh_key.me.fingerprint]
  user_data = templatefile("${path.module}/cloud-init.tftpl", {
    node  = each.key
    image = var.image
    env = join("\n", [
      "COMBD_NODE=${each.key}",
      "COMBD_PEERS=${local.peers}",
      "COMBD_PUBLIC_URLS=${local.public}",
      "COMBD_WIRE_CERTS=/etc/combd/certs",
      "COMBD_REPLICAS=2",
      "COMBD_THREADS=1",
      "COMBD_DATA=/data",
      "COMBD_LEASE_MS=3000",
      "COMBD_S3=https://${var.cloudflare_account_id}.r2.cloudflarestorage.com",
      "COMBD_S3_BUCKET=${var.r2_bucket}",
      "COMBD_S3_KEY=${var.r2_key}",
      "COMBD_S3_SECRET=${var.r2_secret}",
      "COMBD_S3_REGION=auto",
      "COMBD_ADMIN_PORT=9443",
      "COMBD_ADMIN_TOKENS=/etc/combd/tokens",
      "COMBD_MASTER_KEY_FILE=/etc/combd/master.key",
      # Every node keeps its peers' promises (the version this fleet started
      # on does), so owners may answer reads under a lease.
      "COMBD_READ_LEASE=1",
      # HTTPS ended here, on 443 (single mode); 80 sends plain HTTP there.
      "COMBD_TLS_CERT=/etc/combd/tls/cert.pem",
      "COMBD_TLS_KEY=/etc/combd/tls/key.pem",
      "COMBD_REDIRECT_PORT=80",
      # What combd may use of the droplet's 1 GB: Docker and the OS have the
      # rest. Past 85% it reclaims, past 95% it refuses new work, so the
      # kernel never has to kill it.
      "COMBD_MEMORY_BUDGET=700M",
      "",
    ])
    ca       = file("${local.secrets}/certs/ca.pem")
    cert     = file("${local.secrets}/certs/node${each.key}.pem")
    key      = file("${local.secrets}/certs/node${each.key}.key")
    tokens   = file("${local.secrets}/tokens")
    master   = file("${local.secrets}/master.key")
    tls_cert = "${acme_certificate.arena.certificate_pem}${acme_certificate.arena.issuer_pem}"
    tls_key  = acme_certificate.arena.private_key_pem
    update   = file("${path.module}/combd-update.sh")
    tune     = file("${path.module}/combd-tune.sh")
  })
  # Running nodes keep what they started with: a change here is for new
  # droplets (the image itself rolls out through combd-update).
  lifecycle {
    ignore_changes = [user_data]
  }
}

resource "digitalocean_project_resources" "arena" {
  project   = data.digitalocean_project.project.id
  resources = [for d in digitalocean_droplet.node : d.urn]
}

# HTTP, HTTPS, and the admin API for everyone; the wire only between the
# nodes; SSH only from you.
resource "digitalocean_firewall" "arena" {
  name        = "combd-arena"
  droplet_ids = [for d in digitalocean_droplet.node : d.id]
  inbound_rule {
    protocol         = "tcp"
    port_range       = "80"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  inbound_rule {
    protocol         = "tcp"
    port_range       = "443"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  inbound_rule {
    protocol           = "tcp"
    port_range         = "7100"
    source_droplet_ids = [for d in digitalocean_droplet.node : d.id]
  }
  # The admin API (TLS, a token checked before any body is read), for
  # deploys from CI too.
  inbound_rule {
    protocol         = "tcp"
    port_range       = "9443"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = [var.admin_cidr]
  }
  inbound_rule {
    protocol         = "icmp"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  outbound_rule {
    protocol              = "tcp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
  outbound_rule {
    protocol              = "udp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
  outbound_rule {
    protocol              = "icmp"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
}

# Each node by name: the wire's peers and the arena's ping use these.
resource "cloudflare_record" "node" {
  for_each = digitalocean_droplet.node
  zone_id  = data.cloudflare_zone.zone.id
  name     = "n${each.key}.${var.name}"
  content  = each.value.ipv4_address
  type     = "A"
  proxied  = false
  ttl      = 60
}

# The one name: the nearest healthy node.
resource "cloudflare_load_balancer_monitor" "ping" {
  count      = var.load_balancer ? 1 : 0
  account_id = var.cloudflare_account_id
  type       = "https"
  method     = "GET"
  path       = "/_combd/ping"
  port       = 443
  header {
    header = "Host"
    values = [local.host]
  }
  expected_codes = "200"
  interval       = 60
  timeout        = 5
  retries        = 2
  description    = "combd arena"
}

resource "cloudflare_load_balancer_pool" "node" {
  for_each   = var.load_balancer ? digitalocean_droplet.node : {}
  account_id = var.cloudflare_account_id
  name       = "combd-arena-${each.key}-${var.regions[tonumber(each.key)]}"
  latitude   = local.where[var.regions[tonumber(each.key)]][0]
  longitude  = local.where[var.regions[tonumber(each.key)]][1]
  monitor    = cloudflare_load_balancer_monitor.ping[0].id
  origins {
    name    = "n${each.key}"
    address = each.value.ipv4_address
    enabled = true
  }
}

resource "cloudflare_load_balancer" "arena" {
  count            = var.load_balancer ? 1 : 0
  zone_id          = data.cloudflare_zone.zone.id
  name             = local.host
  default_pool_ids = [for k in sort(keys(cloudflare_load_balancer_pool.node)) : cloudflare_load_balancer_pool.node[k].id]
  fallback_pool_id = cloudflare_load_balancer_pool.node["0"].id
  ttl              = 30 # DNS only: not proxied (that is the default)
  steering_policy  = "proximity"
}

# The other apps: each name is all the nodes, round robin.
resource "cloudflare_record" "app" {
  for_each = { for p in setproduct(var.apps, keys(digitalocean_droplet.node)) : "${p[0]}-${p[1]}" => { app = p[0], ip = digitalocean_droplet.node[p[1]].ipv4_address } }
  zone_id  = data.cloudflare_zone.zone.id
  name     = each.value.app
  content  = each.value.ip
  type     = "A"
  proxied  = false
  ttl      = 60
}

# No load balancer: the name is all three nodes, round robin.
resource "cloudflare_record" "arena" {
  for_each = var.load_balancer ? {} : digitalocean_droplet.node
  zone_id  = data.cloudflare_zone.zone.id
  name     = var.name
  content  = each.value.ipv4_address
  type     = "A"
  proxied  = false
  ttl      = 60
}

output "host" {
  value = local.host
}

output "nodes" {
  value = { for k, d in digitalocean_droplet.node : "n${k}.${local.host}" => "${d.ipv4_address} (${d.region})" }
}
