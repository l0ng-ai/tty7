#!/usr/bin/env bash
#
# The GUI must not touch the filesystem or git directly.
#
# Once a workspace can live on a remote machine, a path held by `ui::` or
# `terminal::` is not necessarily a path on *this* box, and `std::path`'s
# fs-backed APIs quietly answer for the wrong machine:
# `canonicalize` walks the local filesystem, `is_absolute` says `false` for
# `/home/me` on Windows, `read_dir` lists the client's disk. Everything that may
# be looking at a workspace path has to go through `ui::host_ops` / the `Host`
# trait, which routes to the local disk or the far side as appropriate.
#
# This script enforces that. It is deliberately *not* the raw grep from the
# contract: run bare, that grep reports 42 hits on a clean tree — not one of them
# git, all of them modules whose paths are local by construction — and a guard
# that always fails is a guard everyone learns to ignore. Two things fix it:
#
#   1. Test bodies are cut off properly. The contract's `grep -v '#\[cfg(test)\]'`
#      only removes the attribute line itself, leaving the whole test module
#      behind it in scope; here the scan stops at the `#[cfg(test)] mod …` that
#      opens the trailing test region.
#   2. An explicit allowlist, keyed on (file, pattern) rather than whole files, so
#      a module exempted for its `.is_absolute()` still trips on a new
#      `std::fs::`. Every entry carries the reason its paths cannot be remote.
#
# Adding an entry is a deliberate act: if the path could ever be a workspace
# path, the answer is `Host`, not an allowlist line.
#
# Usage: bash .github/scripts/check-host-boundary.sh
# Exit 0 = clean, 1 = violations (printed to stderr).

set -euo pipefail

cd "$(dirname "$0")/../.."

ROOTS=(src/ui src/terminal)

# The forbidden constructs, as extended-regex alternatives.
PATTERN='std::fs::|Command::new\("git"\)|\.canonicalize\(\)|\.is_absolute\(\)'

# Allowlist entries are `<path>|<literal pattern>`, one per line. `<path>|*`
# exempts a file wholesale (used only for the boundary module itself). The
# pattern side is matched as a plain substring of the offending line.
ALLOW=$(
    cat <<'EOF'
# The host boundary itself: every routed filesystem call lands here by design.
src/ui/host_ops.rs|*

# Themes and presets are app-owned files under the local config dir
# (`themes_dir()`), never a workspace path — a remote workspace does not carry
# the user's color schemes with it.
src/ui/presets.rs|std::fs::
src/ui/presets.rs|.is_absolute()
src/ui/app.rs|std::fs::create_dir_all

# The editor's record of which files each tab had open is app state under the
# local config dir, like the themes above — not a file in any workspace.
src/ui/editor_session.rs|std::fs::read

# Reading a private key off *this* machine to hash it into a keychain account
# (`core::keychain`). The key is the client's credential; the far side never
# sees the file, only the resulting auth.
src/ui/ssh_prompt.rs|std::fs::read
src/ui/ssh_connect.rs|std::fs::read
src/ui/settings.rs|std::fs::read
# The host editor asking which of those keys is on this machine before it offers
# a passphrase box for one — the same client-side key, never a workspace path.
src/ui/settings.rs|std::fs::metadata

# Shell history lives in the local user's home (`~/.zsh_history` &co.) and backs
# this app's own history search. A remote pane's history is the remote shell's
# business, read over the wire, not through here.
src/terminal/history.rs|std::fs::

# Completion is already remote-aware: a remote pane is signalled by `cwd: None`,
# which disables exactly these local-filesystem candidate sources in favour of
# `remote_path_request` / `remote_path_candidates`. The `$PATH` scan is likewise
# this machine's `$PATH`, deliberately withheld from remote panes.
src/terminal/completion.rs|std::fs::read_dir
src/terminal/completion.rs|.is_absolute()

# Bundled completion specs shipped in `assets/completions`, resolved from the
# app bundle / dev manifest dir. Program data, not user or workspace data.
src/terminal/signature.rs|std::fs::read_to_string

# Opening a path scraped out of terminal output resolves it against the local
# cwd — a local-pane affordance; `resolve_existing_path` declines when there is
# no local cwd.
src/terminal/search.rs|.is_absolute()

# Clipboard-image paste stages the bytes into the OS temp dir so an agent TUI
# can be handed a path. Always `std::env::temp_dir()` on this machine.
src/terminal/view.rs|std::fs::create_dir_all
src/terminal/view.rs|std::fs::write
# The source side of a paste/drop upload into a remote pane: the paths the
# clipboard or desktop hands over are on this machine by construction, so
# asking whether one is a directory is a local read. The remote side of the
# copy goes over SFTP.
src/terminal/view.rs|std::fs::metadata

# Asking whether a file would be *launched* rather than shown before handing it
# to the desktop opener. Only reachable behind `host_id.is_local()` — a path on
# another machine takes the earlier arm and never gets here.
src/ui/code_editor.rs|std::fs::metadata

# The source side of a file drop. What the desktop hands over is by
# construction a path on the desktop's own machine, so reading it is a local
# read even when the tree being dropped on is remote — the destination side of
# that copy goes through `Host`, and the one `std::fs::copy` that touches a
# destination sits inside a branch already gated on `host.id().is_local()`.
src/ui/file_copy.rs|std::fs::

# The code editor's language servers run on this machine and only ever take a
# local buffer: `OpenFile::local` refuses anything whose host is not local, so
# every path `ui::lsp` holds is on this disk. It reads files a server points
# into that are not open (to measure a column, to apply a rename to disk),
# looks for project markers above a file (`Cargo.toml`, `package.json`, …),
# and checks a server binary is executable. A remote workspace gets language
# servers by running them over there, not by routing these reads.
src/ui/lsp/editor.rs|std::fs::read_to_string
src/ui/lsp/locations.rs|std::fs::read_to_string
src/ui/lsp/mod.rs|std::fs::read_to_string
src/ui/lsp/servers.rs|std::fs::metadata
src/ui/lsp/servers.rs|std::fs::read_to_string
# Its tests live in their own file (not a trailing `mod tests`), writing
# fixtures into a local temp dir.
src/ui/lsp/tests.rs|std::fs::write
src/ui/lsp/tests.rs|.canonicalize()
EOF
)

# Where the trailing `#[cfg(test)] mod …` starts, if any. Line numbers are
# preserved because only the tail is dropped.
body_of() {
    local file=$1 cut
    # `attr` starts unset, which awk reads as 0 — so a file whose *first* line
    # is `mod something` used to match `attr == NR - 1` and cut the body at
    # line 0, leaving `head -n -1` to error out and the file to be scanned as
    # empty. Two files opened that way, and the guard was blind to both.
    cut=$(awk '
        BEGIN                { attr = -1 }
        /^#\[cfg\(test\)\]$/ { attr = NR }
        /^mod [A-Za-z_]/     { if (attr == NR - 1) { print NR - 1; exit } }
    ' "$file")
    if [ -n "$cut" ]; then
        head -n "$((cut - 1))" "$file"
    else
        cat "$file"
    fi
}

allowed() {
    local file=$1 line=$2 entry path pat
    while IFS= read -r entry; do
        case "$entry" in '' | '#'*) continue ;; esac
        path=${entry%%|*}
        pat=${entry#*|}
        [ "$path" = "$file" ] || continue
        if [ "$pat" = '*' ] || [ "${line#*"$pat"}" != "$line" ]; then
            return 0
        fi
    done <<<"$ALLOW"
    return 1
}

# A guard that scans nothing reports success, which is the one failure mode worse
# than a noisy guard. Fail loudly if a root has moved out from under us.
for root in "${ROOTS[@]}"; do
    if [ ! -d "$root" ]; then
        echo "check-host-boundary: scan root '$root' does not exist (moved? renamed?)" >&2
        exit 2
    fi
done

violations=0
scanned=0
while IFS= read -r file; do
    scanned=$((scanned + 1))
    while IFS= read -r hit; do
        lineno=${hit%%:*}
        text=${hit#*:}
        # Prose, not code.
        case "$(printf '%s' "$text" | sed 's/^[[:space:]]*//')" in '//'*) continue ;; esac
        if allowed "$file" "$text"; then
            continue
        fi
        printf '%s:%s:%s\n' "$file" "$lineno" "$text" >&2
        violations=$((violations + 1))
    done < <(body_of "$file" | grep -nE "$PATTERN" || true)
done < <(find "${ROOTS[@]}" -name '*.rs' | sort)

if [ "$scanned" -lt 10 ]; then
    echo "check-host-boundary: only $scanned files scanned — the roots look wrong" >&2
    exit 2
fi

if [ "$violations" -ne 0 ]; then
    cat >&2 <<'EOF'

--------------------------------------------------------------------------------
The GUI reached the filesystem/git directly.

A path in `ui::` or `terminal::` may belong to a remote workspace, where these
calls answer for the wrong machine. Route it through `ui::host_ops` / `Host`
instead (`Host::read_dir`, `join`, `is_absolute`, `canonicalize`, …).

If the path genuinely cannot be remote — app config, bundled assets, the local
temp dir — add it to the allowlist in this script with the reason why.
--------------------------------------------------------------------------------
EOF
    exit 1
fi

echo "host boundary clean: $scanned files in ${ROOTS[*]}, no direct fs/git calls"
