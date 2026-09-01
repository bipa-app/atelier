#!/bin/sh
set -eu

force=0
if [ "${1:-}" = "--force" ]; then
    force=1
    shift
fi

if [ "$#" -ne 3 ]; then
    printf 'usage: %s [--force] suggestion|bug|docs TITLE BODY_FILE
' "$0" >&2
    exit 64
fi

kind=$1
title=$2
body_file=$3
repo=${ATELIER_FEEDBACK_REPO:-bipa-app/atelier}

case "$kind" in
    suggestion) prefix='Suggestion' ;;
    bug) prefix='Bug' ;;
    docs) prefix='Docs' ;;
    *)
        printf 'unknown feedback kind: %s
' "$kind" >&2
        exit 64
        ;;
esac

if [ ! -s "$body_file" ]; then
    printf 'issue body is missing or empty: %s
' "$body_file" >&2
    exit 66
fi

command -v gh >/dev/null 2>&1 || {
    printf 'gh is required to search and open Atelier issues
' >&2
    exit 69
}

if [ "$force" -eq 0 ]; then
    open_matches=$(gh issue list --repo "$repo" --state open --search "$title in:title" --limit 10)
    closed_matches=$(gh issue list --repo "$repo" --state closed --search "$title in:title" --limit 10)
    if [ -n "$open_matches$closed_matches" ]; then
        printf 'possible duplicate issues found:
%s
%s
' "$open_matches" "$closed_matches" >&2
        printf 'comment on a match, or rerun with --force after proving this is distinct
' >&2
        exit 2
    fi
fi

gh issue create     --repo "$repo"     --title "$prefix: $title"     --body-file "$body_file"
