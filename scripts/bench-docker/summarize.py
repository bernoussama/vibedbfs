#!/usr/bin/env python3
"""Summarize dbfs benchmark results as medians over runs.

Usage:
  summarize.py <dir> [<dir2>]

Each <dir> holds bench records, fio results, or both.

  bench records  every *.jsonl file under <dir>, recursively, except inside a
                 raw/ directory. Each non-blank line is a JSON object with
                 "workload", "label" (default: the file stem), "run" and
                 numeric fields. compare.sh writes <dir>/results/<config>.jsonl
                 and unlabeled copies under <dir>/raw/, which are skipped;
                 bench.sh writes <label>.jsonl, possibly directly in <dir>.
  fio results    <dir>/fio/<label>/<job>-<round>.json, raw
                 `fio --output-format=json` output. The workload is
                 "fio:" + the job name, so it never collides with a bench
                 workload of the same name.

Metrics: MiB/s and IOPS (higher is better), s and s:<key> (lower is better).
Each cell is the median over its runs. A cell shows n=<k> when it has fewer
runs than the matrix's most common run count.

With one dir, prints its matrix. With two, prints the first dir's matrix, the
second dir's matrix, then a ratio matrix where >1 means the second dir is
better.

Exit status: 0 on success; 1 if a dir has no usable results or a file is
malformed; 2 on usage errors.
"""

import argparse
import json
import os
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path

LABEL_ORDER = [
    "ext4", "btrfs", "pt-ext4", "pt-ext4-user", "pt-btrfs", "pt-btrfs-user",
    "dbfs-ext4", "dbfs-ext4-kc", "dbfs-btrfs", "dbfs-btrfs-kc",
]
WORKLOAD_ORDER = [
    "seqwrite", "seqwrite-long", "randwrite", "randwrite-fsync", "seqread",
    "randread", "seqread-cold", "randread-cold", "metadata", "bigdir",
    "smallfiles", "tree-cold-stat", "tree-cold-rm",
]
METRIC_ORDER = {"MiB/s": 0, "IOPS": 1, "s": 2}
HIGHER_IS_BETTER = ("MiB/s", "IOPS")
RESERVED_KEYS = ("workload", "label", "run")
FIO_PREFIX = "fio:"
RAW_DIR = "raw"
MIB = 2 ** 20


def fail(message):
    print(message, file=sys.stderr)
    sys.exit(1)


def reject_constant(name):
    raise ValueError(f"{name} is not valid JSON")


def number(value):
    """The value if it is a JSON int or float (not a bool), else None."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value


def bench_metric(key, value):
    """Metric name for one bench field, or None if the field is not a metric."""
    if key in RESERVED_KEYS or number(value) is None:
        return None
    if key == "iops":
        return "IOPS"
    if not isinstance(value, float):
        return None  # int fields other than iops are counts
    if key == "mib_s":
        return "MiB/s"
    if key == "secs":
        return "s"
    return "s:" + key


def read_bench_file(path, values):
    """Add the metrics of every record in one .jsonl file to values."""
    with open(path, "rb") as fh:
        for lineno, raw in enumerate(fh, 1):
            if not raw.strip():
                continue
            try:
                record = json.loads(raw.decode("utf-8"), parse_constant=reject_constant)
            except json.JSONDecodeError as exc:
                fail(f"{path}:{lineno}: {exc.msg}")
            except ValueError as exc:
                fail(f"{path}:{lineno}: {exc}")
            if not isinstance(record, dict) or not isinstance(record.get("workload"), str):
                fail(f"{path}:{lineno}: not a bench record (no string 'workload')")
            label = record.get("label", path.stem)
            if not isinstance(label, str):
                fail(f"{path}:{lineno}: 'label' is not a string")
            for key, value in record.items():
                metric = bench_metric(key, value)
                if metric is not None:
                    values[(label, record["workload"], metric)].append(float(value))


def fio_direction(job):
    """'read' or 'write' for one fio job, chosen from its rw option."""
    options = job.get("job options")
    rw = options.get("rw", "") if isinstance(options, dict) else ""
    if not isinstance(rw, str):
        rw = ""
    return "read" if "read" in rw and "write" not in rw else "write"


def fio_block(job, direction):
    block = job.get(direction)
    return block if isinstance(block, dict) else {}


def fio_bytes_per_sec(block):
    bw_bytes = number(block.get("bw_bytes"))
    if bw_bytes is None:
        bw_kib = number(block.get("bw"))
        if bw_kib is not None:
            bw_bytes = bw_kib * 1024  # older fio: bw is KiB/s
    return bw_bytes


def fio_group_totals(entries, direction):
    """(bytes per second, IOPS) for one fio job group; either may be None.

    One entry is already the group total: group_reporting, or numjobs=1.
    Without group_reporting, fio writes one entry per thread, all with the
    same jobname and groupid. Each thread's bw and iops cover only its own
    runtime, and fio's group figure (its "Run status group" line) is the total
    I/O over the longest runtime. Summing the per-thread bw_bytes overstates
    that: on one run fio printed 1524 MiB/s, and the per-thread bw_bytes summed
    to 1881. Weighting each thread by its runtime gives the group figure:
    sum(bw * runtime) / max(runtime), which equals sum(io_bytes) / max(runtime).
    The same holds for iops.
    """
    blocks = [fio_block(entry, direction) for entry in entries]
    if len(entries) == 1:
        return fio_bytes_per_sec(blocks[0]), number(blocks[0].get("iops"))
    runtimes = [number(block.get("runtime")) for block in blocks]
    if any(rt is None or rt <= 0 for rt in runtimes):
        return None, None
    longest = max(runtimes)
    bws = [fio_bytes_per_sec(block) for block in blocks]
    iopss = [number(block.get("iops")) for block in blocks]
    bw = None if None in bws else sum(v * rt for v, rt in zip(bws, runtimes)) / longest
    iops = None if None in iopss else sum(v * rt for v, rt in zip(iopss, runtimes)) / longest
    return bw, iops


def read_fio_file(path, label, values):
    """Add the job groups of one fio --output-format=json file to values."""
    try:
        with open(path, encoding="utf-8") as fh:
            doc = json.load(fh, parse_constant=reject_constant)
    except json.JSONDecodeError as exc:
        fail(f"{path}:{exc.lineno}: {exc.msg}")
    except ValueError as exc:
        fail(f"{path}:1: {exc}")
    jobs = doc.get("jobs") if isinstance(doc, dict) else None
    if not isinstance(jobs, list):
        fail(f"{path}: no 'jobs' list; not fio JSON output")
    groups = defaultdict(list)
    for job in jobs:
        if not isinstance(job, dict) or not isinstance(job.get("jobname"), str):
            fail(f"{path}: fio job entry without a string 'jobname'")
        groups[(job.get("groupid", 0), job["jobname"])].append(job)
    for (_, jobname), entries in groups.items():
        bw, iops = fio_group_totals(entries, fio_direction(entries[0]))
        if bw is not None:
            values[(label, FIO_PREFIX + jobname, "MiB/s")].append(bw / MIB)
        if iops is not None:
            values[(label, FIO_PREFIX + jobname, "IOPS")].append(float(iops))


def load_dir(name):
    """{(label, workload, metric): [values]} for one results dir; exits on error."""
    root = Path(name)
    if not root.is_dir():
        reason = "not a directory" if root.exists() else "no such directory"
        fail(f"no results under {name}: {reason}")
    values = defaultdict(list)
    for path in sorted(root.rglob("*.jsonl")):
        if not path.is_file() or RAW_DIR in path.relative_to(root).parts[:-1]:
            continue
        read_bench_file(path, values)
    fio_root = root / "fio"
    if fio_root.is_dir():
        for path in sorted(fio_root.glob("*/*.json")):
            if path.is_file():
                read_fio_file(path, path.parent.name, values)
    if not values:
        fail(f"no results under {name}")
    return values


def medians(values):
    """{(label, workload, metric): (median, n)}."""
    return {key: (statistics.median(vals), len(vals)) for key, vals in values.items()}


def label_key(label):
    if label in LABEL_ORDER:
        return (0, LABEL_ORDER.index(label), "")
    return (1, 0, label)


def row_key(row):
    """Sort key for a (workload, metric) row."""
    workload, metric = row
    if workload in WORKLOAD_ORDER:
        wkey = (0, WORKLOAD_ORDER.index(workload), "")
    elif workload.startswith(FIO_PREFIX):
        wkey = (1, 0, workload)
    else:
        wkey = (2, 0, workload)
    if metric in METRIC_ORDER:
        mkey = (METRIC_ORDER[metric], "")
    else:
        mkey = (3, metric)  # s:<key>, sorted by key
    return (wkey, mkey)


def fmt_value(metric, value):
    if metric == "MiB/s":
        return f"{value:.1f}"
    if metric == "IOPS":
        return f"{value:.0f}"
    return f"{value:.3f}"  # s and s:<key>


def common_run_count(med):
    """The most common n in a matrix; ties go to the larger n."""
    counts = Counter(n for _, n in med.values())
    return max(counts, key=lambda n: (counts[n], n))


def render(title, rows, labels, cell):
    """Lines of one matrix. rows are (workload, metric); cell(row, label) gives text."""
    grid = [[cell(row, label) for label in labels] for row in rows]
    wl_w = max(len("workload"), *(len(wl) for wl, _ in rows))
    m_w = max(len("metric"), *(len(metric) for _, metric in rows))
    # Widen a column past max(10, len(label)) only when a cell is wider.
    widths = [max(10, len(label), *(len(cells[j]) for cells in grid))
              for j, label in enumerate(labels)]
    lines = [title]
    lines.append(f"{'workload':<{wl_w}}  {'metric':<{m_w}}"
                 + "".join(f"  {label:>{w}}" for label, w in zip(labels, widths)))
    for (wl, metric), cells in zip(rows, grid):
        lines.append(f"{wl:<{wl_w}}  {metric:<{m_w}}"
                     + "".join(f"  {text:>{w}}" for text, w in zip(cells, widths)))
    return lines


def dir_block(name, med):
    labels = sorted({label for label, _, _ in med}, key=label_key)
    rows = sorted({(wl, metric) for _, wl, metric in med}, key=row_key)
    common = common_run_count(med)

    def cell(row, label):
        hit = med.get((label, row[0], row[1]))
        if hit is None:
            return "-"
        value, n = hit
        text = fmt_value(row[1], value)
        return text if n >= common else f"{text} n={n}"

    return render(f"== {name} (median over runs)", rows, labels, cell)


def basename(name):
    return Path(os.path.abspath(name)).name


def ratio_block(name1, med1, name2, med2):
    keys = set(med1) | set(med2)
    labels = sorted({label for label, _, _ in keys}, key=label_key)
    rows = sorted({(wl, metric) for _, wl, metric in keys}, key=row_key)

    def cell(row, label):
        a = med1.get((label, row[0], row[1]))
        b = med2.get((label, row[0], row[1]))
        if a is None or b is None or a[0] == 0 or b[0] == 0:
            return "-"
        if row[1] in HIGHER_IS_BETTER:
            ratio = b[0] / a[0]
        else:
            ratio = a[0] / b[0]
        return f"{ratio:.2f}x"

    base1, base2 = basename(name1), basename(name2)
    title = f"== ratio {base2} vs {base1} (>1 means {base2} is better)"
    return render(title, rows, labels, cell)


def main(argv=None):
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("dirs", nargs="+", metavar="dir",
                        help="results directory; with two, the second is compared with the first")
    args = parser.parse_args(argv)
    if len(args.dirs) > 2:
        parser.error("expected one or two directories")
    meds = [medians(load_dir(name)) for name in args.dirs]
    if len(args.dirs) == 1:
        lines = dir_block(args.dirs[0], meds[0])
    else:
        lines = (dir_block(args.dirs[0], meds[0])
                 + dir_block(args.dirs[1], meds[1])
                 + ratio_block(args.dirs[0], meds[0], args.dirs[1], meds[1]))
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
