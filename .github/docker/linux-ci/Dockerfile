FROM ubuntu:24.04@sha256:4fbb8e6a8395de5a7550b33509421a2bafbc0aab6c06ba2cef9ebffbc7092d90

ENV DEBIAN_FRONTEND=noninteractive

RUN sed -i 's|http://archive.ubuntu.com/ubuntu/|https://snapshot.ubuntu.com/ubuntu/20260719T000000Z/|g; s|http://security.ubuntu.com/ubuntu/|https://snapshot.ubuntu.com/ubuntu/20260719T000000Z/|g' /etc/apt/sources.list.d/ubuntu.sources \
  && apt-get update \
  && apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  file \
  git \
  libgtk-3-dev \
  libwebkit2gtk-4.1-dev \
  make \
  pkg-config \
  unzip \
  xz-utils \
  zip \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
