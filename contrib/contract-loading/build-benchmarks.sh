#!/usr/bin/env bash
# Copyright (C) 2026 Stacks Open Internet Foundation
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <http://www.gnu.org/licenses/>.

# Build an unchanged control and the integrated Postcard candidate with the same profile.
# No node is started and no chainstate is opened.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "Usage: $0 BASELINE_REVISION [OUTPUT_DIRECTORY]" >&2
    exit 2
fi
repo=$(git rev-parse --show-toplevel)
baseline_revision=$(git rev-parse "${1}^{commit}")
output=${2:-"$repo/target/contract-loading-live"}
mkdir -p "$output"
output=$(cd "$output" && pwd)
baseline_dir=$(mktemp -d "${TMPDIR:-/tmp}/stacks-contract-baseline.XXXXXX")
cleanup() {
    git -C "$repo" worktree remove "$baseline_dir" || true
}
trap cleanup EXIT
git -C "$repo" worktree add --detach "$baseline_dir" "$baseline_revision"

(
    cd "$baseline_dir"
    cargo build --locked --release -p stacks-node --target-dir "$repo/target"
)
cp "$repo/target/release/stacks-node" "$output/stacks-node-original"

cd "$repo"
cargo build --locked --release -p stacks-node --target-dir "$repo/target"
cp target/release/stacks-node "$output/stacks-node-postcard"

python3 - "$repo" "$output" "$baseline_revision" <<'PY'
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys

repo, output, baseline = sys.argv[1:]
def git(*args):
    return subprocess.check_output(['git', '-C', repo, *args])
paths = set(git('ls-files', '-z').split(b'\0'))
paths.update(git('ls-files', '--others', '--exclude-standard', '-z').split(b'\0'))
source = hashlib.sha256()
for raw in sorted(paths - {b''}):
    name = os.fsdecode(raw)
    if not (name.endswith('.rs') or Path(name).name in {'Cargo.toml', 'Cargo.lock', 'rust-toolchain', 'rust-toolchain.toml'} or name.startswith('.cargo/')):
        continue
    path = Path(repo, name)
    source.update(raw + b'\0')
    source.update(path.read_bytes() if path.is_file() else b'<deleted>')
binaries = {}
for name, features in [('original', []), ('postcard', [])]:
    path = Path(output, 'stacks-node-' + name)
    with path.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    binaries[name] = {'path': str(path), 'sha256': digest, 'bytes': path.stat().st_size, 'features': features}
manifest = {
    'created_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'baseline_commit': baseline,
    'candidate_head': git('rev-parse', 'HEAD').decode().strip(),
    'candidate_rust_and_cargo_sha256': source.hexdigest(),
    'candidate_has_uncommitted_changes': bool(git('status', '--porcelain').strip()),
    'profile': 'release', 'platform': platform.platform(), 'machine': platform.machine(),
    'rustc': subprocess.check_output(['rustc', '-vV']).decode(),
    'rustflags': os.environ.get('RUSTFLAGS', ''),
    'binaries': binaries,
}
Path(output, 'build-info.json').write_text(json.dumps(manifest, indent=2) + '\n')
PY
