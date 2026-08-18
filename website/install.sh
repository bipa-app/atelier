#!/bin/sh
# atelier installer - fetches the latest release's cargo-dist installer.
# Usage: curl -fsSL https://atelier-ws.dev/install.sh | sh
set -euf
echo "installing atelier (latest release from github.com/bipa-app/atelier)..."
curl -fsSL https://github.com/bipa-app/atelier/releases/latest/download/atelier-ws-installer.sh | sh
