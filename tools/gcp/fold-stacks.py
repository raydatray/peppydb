"""Fold bpftrace stack maps into flame-graph input (plan 0024).

Usage: python3 fold-stacks.py [--maps MAPS] < bpftrace-output > folded

Reads the prints of a map keyed by (kernel stack, user stack), such as
tools/gcp/offcpu.bt's @offcpu_us, and writes one line per distinct stack:
"root;...;leaf VALUE", with values summed over every print. User frames
come first (outermost first), then kernel frames, marked "_[k]". The output
opens in speedscope, inferno-flamegraph, or flamegraph.pl.

bpftrace prints such a key as:

    @offcpu_us[
        kernel_leaf+12
        kernel_root+34
    ,
        user_leaf+56
        user_root+78
    ]: 1234

Two repairs make the stacks readable (first profiled live run, 2026-11-09):

- bpftrace 0.20 does not demangle Rust v0 names (_R...). Mangled names go
  through c++filt, which binutils 2.42 can demangle.
- bpftrace symbolizes user stacks when it prints the map; a frame of a
  process that has exited stays a bare address (0x...). With --maps, a copy
  of the process's /proc/PID/maps, such addresses are resolved with
  addr2line against the mapped files, inlined frames included.

Without c++filt or addr2line, names and addresses are kept as they are.
"""

import os
import re
import struct
import subprocess
import sys

OFFSET = re.compile(r"\+(0x)?[0-9a-fA-F]+$")
ADDRESS = re.compile(r"^0x[0-9a-fA-F]+$")
MANGLED = re.compile(r"^(_R|_ZN)[0-9A-Za-z_$.]+$")
# Crate disambiguators in demangled Rust v0 names: std[6c98fd8553dbae28].
DISAMBIGUATOR = re.compile(r"\[[0-9a-f]{1,16}\]")


def frame_name(line):
    name = OFFSET.sub("", line.strip())
    return name.replace(";", ":") or "[unknown]"


def run_tool(argv, lines):
    """Output lines of a tool fed with LINES, or None if it cannot run."""
    try:
        result = subprocess.run(
            argv,
            input="".join(f"{line}\n" for line in lines),
            capture_output=True,
            text=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return result.stdout.splitlines()


def demangle(names):
    """A map from each mangled name to its demangled form."""
    mangled = sorted({name for name in names if MANGLED.match(name)})
    output = run_tool(["c++filt"], mangled)
    if output is None or len(output) != len(mangled):
        return {}
    return {name: DISAMBIGUATOR.sub("", text) for name, text in zip(mangled, output)}


def read_maps(path):
    """(start, end, file offset, file) for each file-backed mapping."""
    maps = []
    with open(path) as lines:
        for line in lines:
            fields = line.split(None, 5)
            if len(fields) == 6 and fields[5].startswith("/"):
                start, end = (int(part, 16) for part in fields[0].split("-"))
                maps.append((start, end, int(fields[2], 16), fields[5].strip()))
    return maps


def load_segments(path):
    """(file offset, virtual address, file size) of each PT_LOAD of an
    ELF64 little-endian file, or [] if it is not one."""
    try:
        with open(path, "rb") as elf:
            header = elf.read(64)
            if header[:6] != b"\x7fELF\x02\x01":
                return []
            phoff, = struct.unpack_from("<Q", header, 32)
            phentsize, phnum = struct.unpack_from("<HH", header, 54)
            elf.seek(phoff)
            table = elf.read(phentsize * phnum)
    except OSError:
        return []
    segments = []
    for index in range(phnum):
        kind, _, offset, vaddr, _, filesz = struct.unpack_from(
            "<IIQQQQ", table, index * phentsize
        )
        if kind == 1:
            segments.append((offset, vaddr, filesz))
    return segments


def file_address(path, offset, segments):
    """The link-time address of a file offset, as addr2line expects."""
    for start, vaddr, size in segments.get(path, []):
        if start <= offset < start + size:
            return offset - start + vaddr
    return offset


def symbolize(addresses, maps):
    """A map from each address string to its frames, innermost first."""
    segments = {}
    by_file = {}
    for text in addresses:
        # A return address points after the call; look up the call itself.
        address = int(text, 16) - 1
        for start, end, offset, path in maps:
            if start <= address < end:
                if path not in segments:
                    segments[path] = load_segments(path)
                target = file_address(path, address - start + offset, segments)
                by_file.setdefault(path, []).append((text, target))
                break
    names = {}
    for path, entries in by_file.items():
        if not os.path.exists(path):
            continue
        output = run_tool(
            ["addr2line", "-e", path, "-a", "-f", "-i", "-C"],
            [hex(target) for _, target in entries],
        )
        if output is None:
            continue
        blocks = []
        for line in output:
            if ADDRESS.match(line):
                blocks.append([])
            elif blocks:
                blocks[-1].append(line)
        for (text, _), block in zip(entries, blocks):
            frames = [
                DISAMBIGUATOR.sub("", name) for name in block[0::2] if name and name != "??"
            ]
            if frames:
                names[text] = frames
    return names


def fold(lines):
    totals = {}
    stacks = None
    for raw in lines:
        line = raw.rstrip("\n")
        stripped = line.strip()
        if stacks is None:
            if stripped.startswith("@") and stripped.endswith("["):
                stacks = [[]]
            continue
        match = re.match(r"^\]:\s*(\d+)$", stripped)
        if match:
            kernel = stacks[0]
            user = stacks[1] if len(stacks) > 1 else []
            frames = [frame_name(f) for f in reversed(user)]
            frames += [frame_name(f) + "_[k]" for f in reversed(kernel)]
            key = ";".join(frames) or "[no stack]"
            totals[key] = totals.get(key, 0) + int(match.group(1))
            stacks = None
        elif stripped == ",":
            stacks.append([])
        elif stripped:
            stacks[-1].append(stripped)
    return totals


def repair(totals, maps):
    """Demangle names and, with MAPS, resolve bare addresses."""
    frames = {frame for key in totals for frame in key.split(";")}
    names = demangle(frames)
    resolved = {}
    if maps:
        addresses = [frame for frame in frames if ADDRESS.match(frame)]
        resolved = symbolize(addresses, maps)
    if not names and not resolved:
        return totals

    def rename(frame):
        if frame in resolved:
            # Innermost first; the folded stack is outermost first.
            return [name.replace(";", ":") for name in reversed(resolved[frame])]
        if frame.endswith("_[k]"):
            return [frame]
        return [names.get(frame, frame).replace(";", ":")]

    repaired = {}
    for key, value in totals.items():
        new_key = ";".join(name for frame in key.split(";") for name in rename(frame))
        repaired[new_key] = repaired.get(new_key, 0) + value
    return repaired


def main():
    args = sys.argv[1:]
    maps = []
    if args[:1] == ["--maps"] and len(args) == 2:
        maps = read_maps(args[1])
    elif args:
        sys.exit("usage: fold-stacks.py [--maps MAPS] < bpftrace-output")
    totals = repair(fold(sys.stdin), maps)
    for key, value in sorted(totals.items(), key=lambda item: -item[1]):
        print(f"{key} {value}")


if __name__ == "__main__":
    main()
