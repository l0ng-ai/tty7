#!/bin/bash
# Usage: install-linux-deps.sh
# Install the system packages a Linux build of tty7 needs, and export the
# gssapi backend selection.
#
# gpui resolves its X11/Wayland/xkb/font backends through `pkg-config` at build
# time, so these have to be on the machine before `cargo build` — a missing one
# surfaces as a build-script failure deep in a dependency, not as anything that
# names the package.
#
# It is a script rather than a copy of the list in each job because more than
# one job needs it (`build` and `clippy`), and GitHub Actions workflows have no
# YAML anchors to share it with. Two hand-maintained copies drift, and the way
# that drift shows up is a green `clippy` and a red `build`, or the reverse.
#
# The same list appears once more, deliberately, in
# docs/getting-started/installation.mdx — contributors building from source need
# to *read* it, not run it. Keep the two in step: a contributor following the
# docs should get exactly the build CI reproduces.
set -euo pipefail

sudo apt-get update
sudo apt-get install -y \
  pkg-config cmake clang \
  libxkbcommon-dev libxkbcommon-x11-dev \
  libfontconfig1-dev libfreetype6-dev \
  libwayland-dev libx11-dev libxcb1-dev \
  libzstd-dev libssl-dev libkrb5-dev

# libgssapi-sys binds a system krb5, and Ubuntu ships MIT rather than Heimdal.
# Without this it guesses, and guesses wrong on the runners.
echo "LIBGSSAPI_IMPL=mit" >> "$GITHUB_ENV"
