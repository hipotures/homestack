set -eu
cloud-init status --wait
test -s /root/.ssh/authorized_keys
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get -o APT::Status-Fd=3 -o Dpkg::Use-Pty=0 -y --no-install-recommends install \
    cloud-init qemu-guest-agent network-manager netplan.io python3-yaml \
    openssh-server e2fsprogs util-linux rsync git sudo
apt-get clean
test "$(id -u user)" = 1000
test "$(id -g user)" = 1000
test "$(awk -F: '$3 >= 1000 && $3 < 65534 {print $1}' /etc/passwd)" = user
test -s /root/.ssh/authorized_keys
test -s /home/user/.ssh/authorized_keys
test -c /dev/virtio-ports/org.qemu.guest_agent.0
systemctl enable qemu-guest-agent NetworkManager ssh
systemctl start qemu-guest-agent
systemctl is-active --quiet qemu-guest-agent
# The next cold boot must use the same renderer as HomeStack's vendor snippet.
install -d -m 0755 /etc/netplan
printf 'network:\n  version: 2\n  renderer: NetworkManager\n' > /etc/netplan/90-homestack-renderer.yaml
chmod 600 /etc/netplan/90-homestack-renderer.yaml
netplan generate
# NetworkManager owns networking from the next boot. Waiting for the old
# networkd backend delays cloud-init and agent startup until its timeout.
systemctl disable systemd-networkd.service systemd-networkd.socket systemd-networkd-wait-online.service
systemctl mask systemd-networkd-wait-online.service
udevadm trigger --subsystem-match=net
python3 -c 'import yaml'
echo 'Reference Gold packages, account, keys and agent prepared'
