#!/bin/bash
#
# End-to-end performance comparison for lms: baseline binary vs current binary,
# with rsync/cp/rm as reference points.
#
#   tools/bench.sh                          # build a master worktree + this checkout, compare them
#   tools/bench.sh --keep                   # keep /tmp/lms-bench (fixtures, logs, built binaries)
#   tools/bench.sh --runs 9                 # more timed runs per case
#   tools/bench.sh --current /path/to/lms   # compare against an already built binary
#   tools/bench.sh --baseline /path/to/lms --current /path/to/lms --no-build
#
# Fixtures mirror the README benchmark table: set A is 7262 small files (~88MB),
# set B is 252 large files (~105MB). Both are regenerated deterministically, so
# repeated invocations measure the same bytes.
#
# Every timed run is checked before its time is accepted: the command must exit 0
# and the destination must hold the same file count and byte total as the source
# (or, for `rm`, be gone). A fast wrong answer is reported as a failure.
#
# Note: fixtures and destinations both live under /tmp, so the numbers are warm
# cache / local filesystem. On a tmpfs they measure CPU + syscall cost only.

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BENCH_DIR="${BENCH_DIR:-/tmp/lms-bench}"

BASELINE_REF="master"
BASELINE_BIN=""
CURRENT_BIN=""
KEEP=0
RUNS=5
WARMUPS=1

function usage {
    sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1 ;;
        --runs) RUNS="${2:?--runs needs a value}"; shift ;;
        --warmups) WARMUPS="${2:?--warmups needs a value}"; shift ;;
        --baseline) BASELINE_BIN="${2:?--baseline needs a path}"; shift ;;
        --current) CURRENT_BIN="${2:?--current needs a path}"; shift ;;
        --baseline-ref) BASELINE_REF="${2:?--baseline-ref needs a ref}"; shift ;;
        --bench-dir) BENCH_DIR="${2:?--bench-dir needs a path}"; shift ;;
        --no-build) NO_BUILD=1 ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage 1 ;;
    esac
    shift
done

echo "== LuminS benchmark =="
echo "repo:      $REPO_DIR"
echo "bench dir: $BENCH_DIR"
echo "cpus:      $(nproc)"
echo "runs:      $RUNS timed (+$WARMUPS warmup, discarded) per case"

# A previous run's worktree holds a git registration; drop it before the tree goes
# away so the repo is not left with stale worktree metadata.
function remove_bench_dir {
    git -C "$REPO_DIR" worktree list --porcelain 2>/dev/null |
        awk -v dir="$BENCH_DIR" '$1 == "worktree" && index($2, dir) == 1 { print $2 }' |
        while read -r wt; do
            git -C "$REPO_DIR" worktree remove --force "$wt" 2>/dev/null
        done
    git -C "$REPO_DIR" worktree prune 2>/dev/null
    rm -rf "$BENCH_DIR"
}

if [ -e "$BENCH_DIR" ]; then
    remove_bench_dir
fi
mkdir -p "$BENCH_DIR"

# ---------------------------------------------------------------- build binaries

BASELINE_REV="$(git -C "$REPO_DIR" rev-parse "$BASELINE_REF")" || {
    echo "cannot resolve baseline ref '$BASELINE_REF'" >&2
    exit 1
}
CURRENT_REV="$(git -C "$REPO_DIR" rev-parse HEAD)"

function build_from_worktree {
    local ref="$1"
    local rev="$2"
    local path
    path="$BENCH_DIR/build-$(echo "$ref" | tr '/' '-')"
    echo "Building baseline ($ref @ ${rev:0:7}) in $path" >&2
    if ! git -C "$REPO_DIR" worktree add --detach "$path" "$rev" >&2; then
        echo "FAILED: git worktree add $path $rev" >&2
        exit 1
    fi
    if ! cargo build --release --locked --manifest-path "$path/Cargo.toml" >&2 2>&1; then
        echo "FAILED: cargo build --release --locked in $path" >&2
        exit 1
    fi
    echo "$path/target/release/lms"
}

if [ -z "$BASELINE_BIN" ]; then
    BASELINE_BIN="$(build_from_worktree "$BASELINE_REF" "$BASELINE_REV")"
    if [ ! -x "$BASELINE_BIN" ]; then
        echo "FAILED: baseline build produced no binary at $BASELINE_BIN" >&2
        exit 1
    fi
fi

if [ -z "$CURRENT_BIN" ]; then
    CURRENT_BIN="$REPO_DIR/target/release/lms"
    if [ -z "${NO_BUILD:-}" ]; then
        echo "Building current (HEAD @ ${CURRENT_REV:0:7}) in $REPO_DIR"
        if ! cargo build --release --locked --manifest-path "$REPO_DIR/Cargo.toml" >&2 2>&1; then
            echo "FAILED: cargo build --release --locked in $REPO_DIR" >&2
            exit 1
        fi
    fi
fi

for bin in "$BASELINE_BIN" "$CURRENT_BIN"; do
    if [ ! -x "$bin" ]; then
        echo "FAILED: $bin is not an executable" >&2
        exit 1
    fi
done

BASELINE_BIN="$(cd "$(dirname "$BASELINE_BIN")" && pwd)/$(basename "$BASELINE_BIN")"
CURRENT_BIN="$(cd "$(dirname "$CURRENT_BIN")" && pwd)/$(basename "$CURRENT_BIN")"

for bin in "$BASELINE_BIN" "$CURRENT_BIN"; do
    echo "binary:    $bin ($(stat -c %s "$bin") bytes, $("$bin" --version 2>&1 | head -1))"
done

BASELINE_LABEL="baseline ${BASELINE_REV:0:7}"
CURRENT_LABEL="current ${CURRENT_REV:0:7}"

# ------------------------------------------------- fixtures, measurement, report

python3 - "$BENCH_DIR" "$RUNS" "$WARMUPS" "$BASELINE_BIN" "$BASELINE_LABEL" \
    "$CURRENT_BIN" "$CURRENT_LABEL" <<'PY'
import os
import random
import shutil
import statistics
import subprocess
import sys
import time

BENCH_DIR, RUNS, WARMUPS, BASE_BIN, BASE_LABEL, CUR_BIN, CUR_LABEL = (
    sys.argv[1],
    int(sys.argv[2]),
    int(sys.argv[3]),
    sys.argv[4],
    sys.argv[5],
    sys.argv[6],
    sys.argv[7],
)

FIXTURES = os.path.join(BENCH_DIR, "fixtures")
WORK = os.path.join(BENCH_DIR, "work")
LOGS = os.path.join(BENCH_DIR, "logs")


# --------------------------------------------------------------------- fixtures
# Deterministic by construction: one seeded RNG per set, fixed iteration order.
def set_a_size(rng):
    """Set A: small files, a few KB up to ~40 KB, skewed small (~12.4 KB mean)."""
    x = rng.random()
    if x < 0.50:
        return rng.randint(2, 8) * 1024
    if x < 0.80:
        return rng.randint(8, 20) * 1024
    if x < 0.95:
        return rng.randint(20, 32) * 1024
    return rng.randint(32, 40) * 1024


def make_set(name, count, size_of, seed):
    root = os.path.join(FIXTURES, name)
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    rng = random.Random(seed)
    total = 0
    for i in range(count):
        data = rng.randbytes(size_of(rng))
        with open(os.path.join(root, "file_%05d.bin" % i), "wb") as fh:
            fh.write(data)
        total += len(data)
    return root, count, total


def generate_fixtures():
    os.makedirs(FIXTURES, exist_ok=True)
    a = make_set("A", 7262, set_a_size, 0xA5E7)
    b = make_set("B", 252, lambda rng: rng.randint(400, 440) * 1024, 0xB33F)
    return a, b


def tree_stats(path):
    """(file count, total bytes) for every regular file under path."""
    files = 0
    total = 0
    stack = [path]
    while stack:
        current = stack.pop()
        try:
            entries = list(os.scandir(current))
        except FileNotFoundError:
            return (-1, -1)
        for entry in entries:
            if entry.is_dir(follow_symlinks=False):
                stack.append(entry.path)
            elif entry.is_file(follow_symlinks=False):
                files += 1
                total += entry.stat(follow_symlinks=False).st_size
    return files, total


# ------------------------------------------------------------------ run helpers
class Case:
    """One (operation, fixture set) combination."""

    def __init__(self, op, label, set_name, src, dest, reference):
        self.op = op
        self.label = label
        self.set_name = set_name
        self.src = src
        self.dest = dest
        self.reference = reference  # (name, argv) or None
        self.results = {}


def prepare_reset(case, kind):
    """Reset state outside the timed region."""
    if os.path.exists(case.dest):
        shutil.rmtree(case.dest)
    if kind == "populate":
        # rm cases need a fresh tree to delete; sync-identical needs a matching one.
        shutil.copytree(case.src, case.dest)


def argv_for(case, name):
    if name == "baseline":
        return [BASE_BIN] + case.op_argv(case.src, case.dest)
    if name == "current":
        return [CUR_BIN] + case.op_argv(case.src, case.dest)
    return case.reference[1]


def verify(case, name, code, elapsed, full_diff):
    """Reject a number we cannot trust."""
    problems = []
    if code != 0:
        problems.append("exit code %d" % code)
    if case.kind == "delete":
        if os.path.exists(case.dest):
            problems.append("destination still exists after rm")
    else:
        want = case.src_stats
        got = tree_stats(case.dest)
        if got != want:
            problems.append(
                "destination mismatch: got %d files/%d bytes, want %d files/%d bytes"
                % (got[0], got[1], want[0], want[1])
            )
        elif full_diff:
            proc = subprocess.run(
                ["diff", "-r", "-q", case.src, case.dest],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            if proc.returncode != 0:
                problems.append("diff -r not clean: %s" % proc.stderr.decode()[:400])
    if problems:
        print(
            "\nFAILED CORRECTNESS CHECK\n  case: %s / set %s / %s\n  %s"
            % (case.label, case.set_name, name, "\n  ".join(problems)),
            file=sys.stderr,
        )
        print("Refusing to report timings. Stopping.", file=sys.stderr)
        sys.exit(2)
    return elapsed


def timed_run(case, name, full_diff):
    prepare_reset(case, case.reset)
    argv = argv_for(case, name)
    log_path = os.path.join(LOGS, "%s-%s-%s.log" % (case.label.replace(" ", "_"), case.set_name, name))
    with open(log_path, "wb") as log:
        start = time.monotonic()
        proc = subprocess.run(argv, stdout=log, stderr=log)
        elapsed = time.monotonic() - start
    if proc.returncode != 0:
        with open(log_path, "rb") as fh:
            tail = fh.read()[-2000:].decode(errors="replace")
        print(
            "\nFAILED: %s exited %d for %s / set %s\n%s"
            % (" ".join(argv[:2]), proc.returncode, name, case.set_name, tail),
            file=sys.stderr,
        )
        sys.exit(2)
    return verify(case, name, proc.returncode, elapsed, full_diff)


# ------------------------------------------------------------------------ driver
def build_cases(a, b):
    cases = []
    for set_name, (src, count, total) in (("A", a), ("B", b)):
        for op, kind, reset in (
            ("sync", "copy", "empty"),
            ("cp", "copy", "empty"),
            ("rm", "delete", "populate"),
            ("sync-same", "copy", "populate"),
        ):
            dest = os.path.join(WORK, "dest-%s-%s" % (set_name, op))
            case = Case(op, op, set_name, src, dest, None)
            case.kind = kind
            case.reset = reset
            case.src_stats = (count, total)
            cases.append(case)
    return cases


def op_argv_factory():
    def op_argv(self, src, dest):
        if self.label == "sync":
            return ["sync", src, dest]
        if self.label == "sync-same":
            return ["sync", src, dest]
        if self.label == "cp":
            return ["cp", src, dest]
        return ["rm", dest]

    return op_argv


Case.op_argv = op_argv_factory()


def reference_name_for(case):
    if case.label in ("sync", "sync-same"):
        return "rsync -r --delete"
    if case.label == "cp":
        return "cp -r"
    return "rm -rf"


def reference_argv(case):
    set_src, set_dest = case.src, case.dest
    if case.label in ("sync", "sync-same"):
        return ["rsync", "-r", "--delete", set_src + "/", set_dest + "/"]
    if case.label == "cp":
        return ["cp", "-r", set_src, set_dest]
    return ["rm", "-rf", set_dest]


def stats(samples_ms):
    """median plus a robust and an extreme view of the run-to-run spread."""
    v = sorted(samples_ms)
    q = statistics.quantiles(v, n=4) if len(v) >= 4 else [v[0], statistics.median(v), v[-1]]
    return {
        "med": statistics.median(v),
        "min": v[0],
        "max": v[-1],
        "p25": q[0],
        "p75": q[2],
        "iqr": q[2] - q[0],
    }


def fmt(ms):
    return "%8.1f ms" % ms


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    shutil.rmtree(LOGS, ignore_errors=True)
    os.makedirs(LOGS)

    a, b = generate_fixtures()
    print("\nfixtures (deterministic):")
    for name, (src, count, total) in (("A", a), ("B", b)):
        print("  set %s: %5d files, %8.1f MiB  (%s)" % (name, count, total / 1048576, src))

    cases = build_cases(a, b)
    for case in cases:
        case.reference = (reference_name_for(case), reference_argv(case))

    print("\nmeasuring (1 warmup discarded, %d timed runs, destination reset between runs,\n           candidate order alternated every round)" % RUNS)

    for case in cases:
        names = ["baseline", "current"]
        reference_name = case.reference[0]
        # Warm up every candidate once, then interleave timed rounds so that machine
        # drift (thermal, background load) cannot favour one binary over the other.
        for name in names + ["reference"]:
            timed_run(case, name, full_diff=False)
        samples = {n: [] for n in names + ["reference"]}
        for round_index in range(RUNS):
            # Alternate the running order every round: a candidate that always runs
            # second inherits whatever state its predecessor leaves behind, which
            # is a bias of the same order as the deltas we are trying to resolve.
            order = names + ["reference"]
            if round_index % 2 == 1:
                order = order[::-1]
            for name in order:
                last = round_index == RUNS - 1 or round_index == RUNS - 2
                samples[name].append(timed_run(case, name, full_diff=last))
        case.results = {n: stats([t * 1000 for t in v]) for n, v in samples.items()}
        case.reference_name = reference_name
        sys.stdout.write(".")
        sys.stdout.flush()
    print()

    # ---------------------------------------------------------------- report
    HEAD = "  %-28s %10s %14s %19s" % ("candidate", "median", "IQR (p25-p75)", "min .. max")
    rows = []
    for case in cases:
        res = case.results
        print("\n== %s | set %s (%d files, %.1f MiB) ==" % (
            case.label, case.set_name, case.src_stats[0], case.src_stats[1] / 1048576))
        print(HEAD)
        for name, label in (("baseline", BASE_LABEL), ("current", CUR_LABEL), ("reference", case.reference_name)):
            s = res[name]
            print("  %-28s %8.1f ms %8.1f ms %8.1f .. %6.1f"
                  % (label if name != "reference" else "ref " + label,
                     s["med"], s["iqr"], s["min"], s["max"]))
        b, c = res["baseline"], res["current"]
        delta = c["med"] - b["med"]
        noise = max(b["iqr"], c["iqr"])
        verdict = "BEYOND spread" if abs(delta) > noise else "within spread (noise)"
        print("  %-28s %+8.1f ms (%+.1f%%)  vs IQR %.1f ms -> %s"
              % ("delta current-baseline", delta, (delta / b["med"] * 100) if b["med"] else 0.0, noise, verdict))
        rows.append((case, b["med"], c["med"], delta, noise))

    width = max(len(c.label) for c in cases)
    print("\n== summary ==")
    print("%-*s | %-3s | %12s | %12s | %16s | %s"
          % (width, "operation", "set", "baseline", "current", "delta", "reference"))
    print("-" * (width + 3 + 5 + 16 + 16 + 20 + 24))
    for case, b_med, c_med, delta, noise in rows:
        ref_label, ref_med = case.reference_name, case.results["reference"]["med"]
        flag = "" if abs(delta) > noise else " (~noise)"
        print("%-*s | %-3s | %9.1f ms | %9.1f ms | %+8.1f ms %+6.1f%%%s | %s %.1f ms"
              % (width, case.label, case.set_name, b_med, c_med, delta,
                 (delta / b_med * 100) if b_med else 0.0, flag, ref_label, ref_med))

    print("\nlegends:")
    print("  sync     = mirror into an absent/empty destination (the README case)")
    print("  sync-same= sync over an identical, already populated destination")
    print("  cp       = copy into an absent destination (same work as `cp -r src dest`)")
    print("  rm       = delete a freshly populated destination tree")
    print("  spread   = the IQR (p25-p75) of the timed runs. A delta smaller than the")
    print("             larger of the two IQRs is indistinguishable from noise;")
    print("             `min .. max` shows the full range, outliers included")
    print("  tmpfs destinations: numbers are warm-cache CPU+syscall cost, not disk I/O")


main()
PY

STATUS=$?
if [ $STATUS -ne 0 ]; then
    echo "benchmark failed (exit $STATUS)" >&2
    [ "$KEEP" -eq 1 ] || echo "logs were under $BENCH_DIR/logs (removed)" >&2
    exit $STATUS
fi

if [ "$KEEP" -eq 1 ]; then
    echo
    echo "kept $BENCH_DIR (fixtures, logs, built worktree)"
else
    echo
    echo "cleaning up $BENCH_DIR"
    remove_bench_dir
fi
