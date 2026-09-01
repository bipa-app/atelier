#!/bin/sh
set -eu

printf 'atelier: '
atelier --version
printf 'os: '
uname -s
printf 'kernel: '
uname -r
printf 'architecture: '
uname -m

if [ -n "${ATELIER_SOURCE_REV:-}" ]; then
    printf 'source revision: %s
' "$ATELIER_SOURCE_REV"
fi
