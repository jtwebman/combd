# Load droplets for a test against a live node: near it (the same region),
# to measure the node, not the internet. Their own state, nothing shared
# with the arena's: `terraform destroy` here can't touch the fleet.
#   TF_VAR_do_token=... terraform apply -var count_of=3
terraform {
  required_version = ">= 1.5"
  required_providers {
    digitalocean = { source = "digitalocean/digitalocean", version = "~> 2.40" }
  }
}

variable "do_token" {
  sensitive = true
}
variable "region" {
  default = "nyc3"
}
variable "count_of" {
  description = "How many: one source address holds about 60,000 connections to one port"
  default     = 3
}
variable "image" {
  default = "ghcr.io/jtwebman/combd-arena:latest"
}

provider "digitalocean" {
  token = var.do_token
}

# The arena's key (made there), so SSH works the same.
data "digitalocean_ssh_key" "me" {
  name = "combd-arena"
}

resource "digitalocean_droplet" "load" {
  count    = var.count_of
  name     = "combd-load-${count.index}"
  region   = var.region
  size     = "s-1vcpu-1gb-amd"
  image    = "ubuntu-24-04-x64"
  tags     = ["combd-load"]
  ssh_keys = [data.digitalocean_ssh_key.me.fingerprint]
  user_data = <<-INIT
    #cloud-config
    package_update: true
    packages: [docker.io]
    write_files:
      - path: /etc/sysctl.d/90-load.conf
        content: |
          net.ipv4.ip_local_port_range = 1024 65535
          fs.file-max = 2000000
          fs.nr_open = 2000000
    runcmd:
      - sysctl --system
      - systemctl enable --now docker
      - docker pull ${var.image}
  INIT
}

output "ips" {
  value = digitalocean_droplet.load[*].ipv4_address
}
