# Load droplets

Machines near a node, to load it from its own region (`RESULTS.md`, "A load
test on a live node"). Their own Terraform state: destroying them here
can't touch the arena.

```sh
TF_VAR_do_token=... terraform apply        # 3 droplets in nyc3 (count_of, region)
# on each: ws-load from the arena's image, at the node's IP on 443:
docker run -d --name load --network host --ulimit nofile=1048576:1048576 \
  -v /etc/ssl/certs/ca-certificates.crt:/ca.pem:ro ghcr.io/jtwebman/combd-arena:latest \
  ./target/release/combd ws-load <node ip>:443 million.combd.dev /ws 35000 \
  '{"t":"range","from":0,"to":500}' /ca.pem
TF_VAR_do_token=... terraform destroy
```

A 1 GB Ubuntu droplet with Docker tracks connections in an 8,192-entry
table: on the load droplets, untrack 443 first (`iptables -t raw -A OUTPUT
-p tcp --dport 443 -j NOTRACK`, and `-A PREROUTING -p tcp --sport 443`),
or they stop near 8,000. One source address holds about 60,000 to a port.
