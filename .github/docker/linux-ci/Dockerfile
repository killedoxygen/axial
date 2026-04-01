FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
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
