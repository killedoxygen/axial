FROM ubuntu:24.04@sha256:4fbb8e6a8395de5a7550b33509421a2bafbc0aab6c06ba2cef9ebffbc7092d90

ENV DEBIAN_FRONTEND=noninteractive

ADD --checksum=sha256:6bac2a01979e210d9eac1d4d56747ec709ea60654744d66705dc3c36e7629e50 \
  https://snapshot.ubuntu.com/ubuntu/20260719T000000Z/pool/main/c/ca-certificates/ca-certificates_20260601~24.04.1_all.deb \
  /tmp/ca-certificates.deb

RUN dpkg-deb --extract /tmp/ca-certificates.deb /tmp/ca-certificates \
  && mkdir -p /etc/ssl/certs \
  && cat /tmp/ca-certificates/usr/share/ca-certificates/mozilla/*.crt > /etc/ssl/certs/ca-certificates.crt \
  && rm -rf /tmp/ca-certificates /tmp/ca-certificates.deb \
  && sed -i 's|http://archive.ubuntu.com/ubuntu/|https://snapshot.ubuntu.com/ubuntu/20260719T000000Z/|g; s|http://security.ubuntu.com/ubuntu/|https://snapshot.ubuntu.com/ubuntu/20260719T000000Z/|g' /etc/apt/sources.list.d/ubuntu.sources \
  && apt-get update --error-on=any \
  && apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  file \
  git \
  libgtk-3-dev \
  libwebkit2gtk-4.1-dev \
  make \
  passwd \
  pkg-config \
  unzip \
  xz-utils \
  zip \
  && groupadd --gid 1001 axial \
  && useradd --uid 1001 --gid axial --create-home --shell /bin/bash axial \
  && install -d --owner axial --group axial /workspace \
  && rm -rf /var/lib/apt/lists/*

ENV HOME=/home/axial
USER 1001:1001
WORKDIR /workspace

RUN test "$(id -u)" = 1001 \
  && test "$(id -g)" = 1001 \
  && test -w "$HOME" \
  && test -w /workspace \
  && pkg-config --exists gtk+-3.0 webkit2gtk-4.1
