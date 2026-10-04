# Disposable sshd target for end-to-end tests of xssh. Destructive tests go here, never to real servers.
#
#   docker build -t xssh-testbed:1 tests/testbed
#   docker run -d --name xssh-testbed -p 127.0.0.1:2222:22 xssh-testbed:1
#   ssh-keygen -t ed25519 -N '' -f kuser_ed25519 && \
#     docker exec -i xssh-testbed sh -c 'cat > /home/kuser/.ssh/authorized_keys' < kuser_ed25519.pub
#   docker rm -f xssh-testbed
#
# Users (password <user>pw1): agent (bash, sudo with password), zuser (zsh, prompt "❯", sudo),
# fuser (fish), buser (busybox ash, prompt "/ # "); kuser: key only.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      openssh-server sudo bash zsh fish busybox tmux vim-tiny python3 rsync procps iproute2 less locales ca-certificates \
    && rm -rf /var/lib/apt/lists/* && mkdir -p /run/sshd && ssh-keygen -A \
    && (locale-gen en_US.UTF-8 zh_CN.UTF-8 zh_CN.GBK 2>/dev/null || true)
RUN useradd -m -s /bin/bash agent && echo 'agent:agentpw1' | chpasswd && usermod -aG sudo agent \
 && useradd -m -s /usr/bin/zsh zuser && echo 'zuser:zuserpw1' | chpasswd && usermod -aG sudo zuser \
 && printf 'PROMPT="%%~ ❯ "\n' > /home/zuser/.zshrc && chown zuser /home/zuser/.zshrc \
 && useradd -m -s /usr/bin/fish fuser && echo 'fuser:fuserpw1' | chpasswd \
 && ln -sf /bin/busybox /bin/ash && echo /bin/ash >> /etc/shells \
 && useradd -m -s /bin/ash buser && echo 'buser:buserpw1' | chpasswd \
 && printf 'export PS1="/ # "\n' > /home/buser/.profile && chown buser /home/buser/.profile \
 && useradd -m -s /bin/bash kuser && passwd -l kuser && install -d -m700 -o kuser -g kuser /home/kuser/.ssh
RUN printf 'PasswordAuthentication yes\nKbdInteractiveAuthentication yes\nUsePAM yes\nMaxSessions 10\n' \
      > /etc/ssh/sshd_config.d/testbed.conf
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
