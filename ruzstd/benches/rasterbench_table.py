#!/usr/bin/env python3
"""Turns the output of the `rasterbench` example into Markdown tables.

    cargo run --release --example rasterbench -- chunks | python ruzstd/benches/rasterbench_table.py

The first table has, for each level, the range and the geometric mean over the files of zstd-rust's time
divided by the C library's (and of the compressed size divided by the C library's). The second has a row
for each file and level. Standard library only.
"""
import math
import re
import sys

ENC = re.compile(
    r"ENC (\S+)\s+L(\d+)\s+C\s+([\d.]+) MB/s ratio ([\d.]+) \| rust\s+([\d.]+) MB/s ratio ([\d.]+) \| time x([\d.]+) size x([\d.]+)"
)
DEC = re.compile(
    r"DEC (\S+)\s+L(\d+)\s+C\s+([\d.]+) MB/s \| rust stream\s+([\d.]+) \(x([\d.]+)\) \| rust all\s+([\d.]+) \(x([\d.]+)\)"
)


def spread(values):
    if not values:
        return "n/a"
    mean = math.exp(sum(math.log(v) for v in values) / len(values))
    return f"{min(values):.2f} to {max(values):.2f} (mean {mean:.2f})"


def main():
    rows = {}
    for line in sys.stdin:
        m = ENC.match(line)
        if m:
            row = rows.setdefault((m[1], int(m[2])), {})
            row.update(c_enc=m[3], c_ratio=m[4], r_enc=m[5], r_ratio=m[6], enc=float(m[7]), size=float(m[8]))
        m = DEC.match(line)
        if m:
            row = rows.setdefault((m[1], int(m[2])), {})
            row.update(c_dec=m[3], r_stream=m[4], stream=float(m[5]), r_all=m[6], whole=float(m[7]))
    if not rows:
        sys.exit("no ENC or DEC lines in the input")
    levels = sorted({level for _, level in rows})
    print("zstd-rust's time and compressed size divided by the C library's, over the files (below 1 is better):")
    print()
    print("| Level | Decode whole frames | Decode as a stream | Compress | Compressed size |")
    print("|---|---|---|---|---|")
    for level in levels:
        of = [row for (_, l), row in rows.items() if l == level]
        cells = [spread([row[k] for row in of if k in row]) for k in ("whole", "stream", "enc")]
        sizes = [row["size"] for row in of if "size" in row]
        size = f"{min(sizes):.3f} to {max(sizes):.3f}" if sizes else "n/a"
        print(f"| {level} | {' | '.join(cells)} | {size} |")
    print()
    print("Throughput in MB/s of uncompressed data; `x` is zstd-rust's time divided by the C library's.")
    print("The frames that are decoded are the C library's.")
    print()
    print(
        "| File | Level | C compress | Rust compress | x | C ratio | Rust ratio | Size x "
        "| C decode | Rust stream | x | Rust whole | x |"
    )
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for (name, level), row in sorted(rows.items()):
        get = lambda k, f="{}": f.format(row[k]) if k in row else ""
        print(
            f"| {name.removesuffix('.raw')} | {level} | {get('c_enc')} | {get('r_enc')} | {get('enc', '{:.2f}')} "
            f"| {get('c_ratio')} | {get('r_ratio')} | {get('size', '{:.3f}')} "
            f"| {get('c_dec')} | {get('r_stream')} | {get('stream', '{:.2f}')} | {get('r_all')} | {get('whole', '{:.2f}')} |"
        )


if __name__ == "__main__":
    main()
