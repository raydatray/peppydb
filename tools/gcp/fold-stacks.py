"""Fold bpftrace stack maps into flame-graph input (plan 0024).

Usage: python3 fold-stacks.py < bpftrace-output > folded

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
"""

import re
import sys

OFFSET = re.compile(r"\+(0x)?[0-9a-fA-F]+$")


def frame_name(line):
    name = OFFSET.sub("", line.strip())
    return name.replace(";", ":") or "[unknown]"


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


def main():
    totals = fold(sys.stdin)
    for key, value in sorted(totals.items(), key=lambda item: -item[1]):
        print(f"{key} {value}")


if __name__ == "__main__":
    main()
