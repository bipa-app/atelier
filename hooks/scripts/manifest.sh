#!/bin/sh
# SessionStart context: when the project is an atelier workspace, hand the
# agent the manifest - the read model an actor consumes first. Anywhere
# else (or without the binary) this says nothing at all.
if [ -d .atelier ] && command -v atelier >/dev/null 2>&1; then
    atelier manifest 2>/dev/null || true
fi
