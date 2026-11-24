#!/usr/bin/env bash
# Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
#
# SPDX-License-Identifier: Apache-2.0
#
# Fails if any tracked file contains an identifying property of the machine
# running this check: its kernel release and version string, host name,
# machine id, boot id or login name. Fixtures, screenshots and examples must
# come from the invented demo machine, never from a real system.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

values=()
add() {
    local v=$1
    # Skip values too short or too generic to identify anything.
    [[ ${#v} -lt 6 ]] && return
    case "$v" in root|ubuntu|debian|localhost|user|runner|admin) return ;; esac
    values+=("$v")
}
add "$(uname -r)"
add "$(uname -v)"
add "$(uname -n)"
add "$(hostname 2>/dev/null || true)"
add "$(cat /etc/machine-id 2>/dev/null || true)"
add "$(cat /proc/sys/kernel/random/boot_id 2>/dev/null || true)"
add "$(id -un 2>/dev/null || true)"

status=0
for v in "${values[@]}"; do
    if git grep -I -n -F -e "$v" -- ':!scripts/check-privacy.sh' >/dev/null 2>&1; then
        echo "check-privacy: a tracked file contains a property of this machine:" >&2
        git grep -I -n -F -e "$v" -- ':!scripts/check-privacy.sh' | sed 's/^/  /' | head -5 >&2
        status=1
    fi
done
# Binary files (e.g. PNG screenshots) are checked by content too.
for v in "${values[@]}"; do
    while IFS= read -r f; do
        if grep -q -a -F -e "$v" "$f"; then
            echo "check-privacy: binary file $f contains a property of this machine" >&2
            status=1
        fi
    done < <(git ls-files | grep -E '\.(png|jpg|gif|svg)$' || true)
done
[[ $status -eq 0 ]] && echo "check-privacy: no machine-identifying data in tracked files (${#values[@]} properties checked)"
exit $status
