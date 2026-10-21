#!/bin/sh
# What a 1 GB node needs before combd, run at every boot (combd-tune.service)
# and safe to run again. The load test found both.
#
# Connection tracking: the kernel's table holds 8192 here, and combd's own
# ports need none of it (no NAT, no stateful rules), so 80 and 443 skip it;
# the limit goes up anyway for whatever else does.
modprobe nf_conntrack
sysctl -qw net.netfilter.nf_conntrack_max=65536
for rule in "PREROUTING -p tcp -m multiport --dports 80,443" "OUTPUT -p tcp -m multiport --sports 80,443"; do
    # shellcheck disable=SC2086
    iptables -t raw -C $rule -j NOTRACK 2>/dev/null || iptables -t raw -A $rule -j NOTRACK
done

# Kernel TLS (COMBD_KTLS): combd hands a connection's session to it.
modprobe tls

# What Ubuntu runs that a node doesn't need: ~95 MB of the 1 GB.
systemctl disable --now -q fwupd fwupd-refresh.timer multipathd multipathd.socket udisks2 ModemManager 2>/dev/null
mkdir -p /etc/systemd/journald.conf.d
if [ ! -f /etc/systemd/journald.conf.d/combd.conf ]; then
    printf '[Journal]\nSystemMaxUse=50M\nRuntimeMaxUse=20M\n' >/etc/systemd/journald.conf.d/combd.conf
    systemctl restart systemd-journald
fi
exit 0
