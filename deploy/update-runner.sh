#!/bin/sh
# Install the latest runner build from GitHub and restart the service. Run as root on the VPS.
set -eu

asset=playground-runner-x86_64-linux
base=https://github.com/bolli24/playground-runner/releases/download/latest

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

curl -fsSL -o "$asset" "$base/$asset"
curl -fsSL -o "$asset.sha256" "$base/$asset.sha256"
sha256sum --check --quiet "$asset.sha256"

install -m 755 "$asset" /usr/local/bin/playground-runner
systemctl restart playground-runner
echo "installed $(sha256sum /usr/local/bin/playground-runner | cut -c1-12), service restarted"
