#!/usr/bin/env bash
# Free disk on a GitHub-hosted ubuntu runner (~14 GB free by default) by
# removing preinstalled SDKs the Rust build never uses. hm-ci.yml.
set -uo pipefail
df -h /
sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc /usr/local/.ghcup \
  /opt/hostedtoolcache/CodeQL /usr/local/share/boost /usr/share/swift \
  /usr/local/share/powershell /usr/local/share/chromium /opt/microsoft || true
df -h /
