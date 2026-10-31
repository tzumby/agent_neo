# From AWS's example: set the hostname from IMDS in place of cloud-init.
echo "enable set-hostname-imds.service" >> /usr/lib/systemd/system-preset/80-amzn-overrides.preset

systemctl preset set-hostname-imds

# gatewayd runs as its own user; group tss lets it ask the NitroTPM for
# attestation documents (see /etc/udev/rules.d/60-tpm.rules).
getent group tss >/dev/null || groupadd --system tss
useradd --system --no-create-home --home-dir / --shell /sbin/nologin --user-group gatewayd
usermod -aG tss gatewayd
chmod 0755 /usr/local/bin/gatewayd

systemctl enable nftables.service gatewayd.service
