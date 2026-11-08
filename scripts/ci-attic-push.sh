#!/usr/bin/env bash
set -euo pipefail

# Keep credentials out of the runner's persistent Attic configuration.
: "${ATTIC_TOKEN:?Set the ATTIC_TOKEN repository secret to a tsk cache publisher token}"
: "${RUNNER_TEMP:?RUNNER_TEMP must be set by CI}"
export XDG_CONFIG_HOME
XDG_CONFIG_HOME=$(mktemp -d "$RUNNER_TEMP/fiach-attic.XXXXXX")
trap 'rm -rf -- "$XDG_CONFIG_HOME"' EXIT

attic login ci https://cache.thesimplekid.dev/ "$ATTIC_TOKEN"
unset ATTIC_TOKEN
attic push ci:tsk --stdin
