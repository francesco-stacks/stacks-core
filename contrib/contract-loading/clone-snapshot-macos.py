#!/usr/bin/env python3
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
"""Clone a stopped snapshot with clonefile(2), with no physical-copy fallback."""

import argparse
import ctypes
import os
from pathlib import Path
import shutil
import sys


def clone_snapshot(source: Path, destination: Path) -> int:
    if sys.platform != "darwin":
        raise ValueError("This helper requires macOS; on Linux use cp --reflink=always.")
    source = source.resolve(strict=True)
    if os.path.lexists(destination):
        raise FileExistsError(f"Destination already exists: {destination}")
    destination = destination.resolve()
    if not source.is_dir():
        raise ValueError("Source must be a directory containing a stopped snapshot.")
    if destination == source or source in destination.parents:
        raise ValueError("Destination must be outside the source directory.")
    if os.path.lexists(destination):
        raise FileExistsError(f"Destination already exists: {destination}")
    if source.stat().st_dev != destination.parent.stat().st_dev:
        raise ValueError("Source and destination must be on the same filesystem.")

    clonefile = ctypes.CDLL(None, use_errno=True).clonefile
    clonefile.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int]
    clonefile.restype = ctypes.c_int
    count = 0

    def clone_directory(src: Path, dst: Path) -> None:
        nonlocal count
        dst.mkdir()
        with os.scandir(src) as entries:
            for entry in entries:
                target = dst / entry.name
                if entry.is_dir(follow_symlinks=False):
                    clone_directory(Path(entry.path), target)
                elif entry.is_file(follow_symlinks=False):
                    if clonefile(os.fsencode(entry.path), os.fsencode(target), 0) != 0:
                        error = ctypes.get_errno()
                        raise OSError(error, os.strerror(error), entry.path)
                    count += 1
                else:
                    # A symlink could send node writes back to the source DB.
                    raise ValueError(f"Snapshot contains a symlink or special file: {entry.path}")
        shutil.copystat(src, dst, follow_symlinks=False)

    clone_directory(source, destination)
    return count


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    try:
        count = clone_snapshot(args.source, args.destination)
    except (OSError, ValueError) as error:
        print(f"Clone failed: {error}. No physical-copy fallback was attempted. "
              "A partial destination may remain.", file=sys.stderr)
        return 1
    print(f"Cloned {count} files to {args.destination}; the source was not modified.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
