#!/usr/bin/env bash
# Prepare a GitHub-hosted ubuntu runner for a big Rust build (hm-ci.yml):
# print its resources, remove preinstalled SDKs the build never uses, and
# with `--swap <GB>` add a swap file (the all-features build of helios-fhir,
# four FHIR versions in one crate, needs more than the runner's 16 GB RAM).
set -uo pipefail
echo "cpus: $(nproc)"; free -g; df -h /
sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc /usr/local/.ghcup \
  /opt/hostedtoolcache/CodeQL /usr/local/share/boost /usr/share/swift \
  /usr/local/share/powershell /usr/local/share/chromium /opt/microsoft || true
if [ "${1:-}" = "--swap" ]; then
  gb=${2:-16}
  sudo fallocate -l "${gb}G" /hm-swapfile && sudo chmod 600 /hm-swapfile &&
    sudo mkswap /hm-swapfile >/dev/null && sudo swapon /hm-swapfile
  free -g
fi
df -h /
