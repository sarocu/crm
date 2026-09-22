# The search database, pinned and pre-configured.
#
#   docker build -f meilisearch/Dockerfile -t crm-meilisearch .
#
# MEILI_MASTER_KEY is deliberately NOT baked in — inject it at runtime from
# HeyoSecret (or the compose environment) so the image carries no secret.

FROM getmeili/meilisearch:v1.53.2

# This base is Alpine/musl, so the guest packages come from apk rather than
# apt. `iproute2` and `openssh` are what a Firecracker guest needs beyond the
# application: /init.sh configures eth0 with `ip` from the kernel's `ip=`
# parameter, and sshd is what backs `heyctl exec` and `heyctl shell`.
# `socat` is what the heyo host uses to reach into the guest for exec and to
# tail its logs; without it a failing VM has nothing to say.
RUN apk add --no-cache openssh iproute2 curl socat \
    && mkdir -p /run/sshd /etc/ssh/sshd_config.d \
    && echo "PermitRootLogin yes" >> /etc/ssh/sshd_config \
    && echo "PermitEmptyPasswords yes" >> /etc/ssh/sshd_config \
    && echo "PasswordAuthentication yes" > /etc/ssh/sshd_config.d/50-heyo.conf \
    && chmod 644 /etc/ssh/sshd_config.d/50-heyo.conf \
    && ssh-keygen -A \
    # busybox `passwd -d` is not in every Alpine build; edit the shadow entry
    # directly when it is missing, so root can log in with no password.
    && { passwd -d root 2>/dev/null || sed -i 's/^root:[^:]*:/root::/' /etc/shadow; }

# PID 1 for the microVM: the kernel boots `init=/init.sh`, and an image without
# it panics before userspace exists. See rootfs/init.sh.
COPY rootfs/init.sh /init.sh
RUN chmod +x /init.sh \
    && mkdir -p /etc/heyo \
    && echo 'crm-meili' > /etc/heyo/hostname \
    && echo 'crm-meili' > /etc/hostname

COPY meilisearch/config.toml /etc/meilisearch/config.toml

# The app-lb start_command: runs Meilisearch with its database, dumps and
# master key on the persistent /workspace. See rootfs/meili-start.sh.
COPY rootfs/meili-start.sh /usr/local/bin/meili-start
RUN chmod +x /usr/local/bin/meili-start

ENV MEILI_CONFIG_FILE_PATH=/etc/meilisearch/config.toml \
    MEILI_ENV=production \
    MEILI_DB_PATH=/data.ms \
    MEILI_HTTP_ADDR=0.0.0.0:7700 \
    MEILI_NO_ANALYTICS=true

VOLUME ["/data.ms"]
EXPOSE 7700

HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=5 \
  CMD curl -fsS http://127.0.0.1:7700/health || exit 1
