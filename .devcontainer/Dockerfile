FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Set `DEVCONTAINER` environment variable to help with orientation
ENV DEVCONTAINER=true

# Set the default editor and visual
ENV EDITOR=nano
ENV VISUAL=nano

# Install basic development tools
RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates \
  tzdata \
  build-essential \
  cmake \
  less \
  sudo \
  psmisc \
  man-db \
  ripgrep \
  fzf \
  unzip \
  gnupg2 \
  jq \
  python3-pip \
  python3-setuptools \
  python3-wheel \
  python3-dev \
  wget \
  curl \
  gh \
  git \
  nano \
  micro \
  ncdu \
  htop \
  screen \
  fontconfig \
  fonts-dejavu-core \
  fonts-liberation \
  magic-wormhole \
  csvkit \
  nvitop \
  bubblewrap \
  libjpeg-dev \
  && apt-get clean && rm -rf /var/lib/apt/lists/*

# (Optional) symlink python -> python3
RUN ln -s /usr/bin/python3 /usr/bin/python

# Install uv (standalone Rust-based Python tool manager)
RUN curl -LsSf https://astral.sh/uv/install.sh | sh

# Install Node.js & Codex
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get update && apt-get install -y nodejs \
    && npm install -g @openai/codex@latest \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

# VSCode if needed
# curl -Lk 'https://code.visualstudio.com/sha/download?build=stable&os=cli-alpine-x64' --output vscode_cli.tar.gz; tar -xf vscode_cli.tar.gz

# R if needed
#RUN apt-get update && apt-get install -y --no-install-recommends \
#  r-base \
#  r-cran-tidyverse \
#  && apt-get clean && rm -rf /var/lib/apt/lists/*
#RUN Rscript -e 'install.packages("languageserver", repos="https://cloud.r-project.org")'
#ENV LANG=C.UTF-8 LC_ALL=C.UTF-8

# Create workspace and config directories and set permissions
RUN mkdir -p /workspace /root/.codex

WORKDIR /workspace
