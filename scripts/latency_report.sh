#!/bin/sh
# latency_report.sh - summarize garage_beam latency instrumentation (GB-6).
#
# Reads `latency_sample` lines (target garage_beam::latency) and reports N, min,
# p50, p95, p99 (nearest-rank) plus the ascending raw samples for the
# edge -> write-issued, write -> read-returned and derived total segments.
# POSIX sh and POSIX awk only; no Python, no bashisms.

set -eu

prog=$(basename "$0")

usage() {
    cat <<EOF
Usage: $prog [LOG_FILE]

Summarize garage_beam latency instrumentation (GB-6). With no LOG_FILE, or
with "-", samples are read from standard input.

Each group lists N, min, p50, p95, p99 and the raw samples ascending:
  (a) edge_to_write_ns   edge -> write issued
  (b) write_to_read_ns   write -> read returned (may be absent)
  (c) total              (a) + (b), for samples that carry a (b) value

Percentiles use the nearest-rank method: sort ascending and take rank
ceil(p/100 * N), 1-based and clamped to 1..N.

Options:
  -h, --help   show this help and exit
EOF
}

case "${1:-}" in
-h | --help)
    usage
    exit 0
    ;;
esac

if [ "$#" -gt 1 ]; then
    usage >&2
    exit 2
fi

report() {
    awk -v source="$input_label" '
    function swap(arr, i, j,    t) {
        t = arr[i]
        arr[i] = arr[j]
        arr[j] = t
    }

    function quicksort(arr, left, right,    i, j, pivot) {
        if (left >= right) return
        i = left
        j = right
        pivot = arr[int((left + right) / 2)]
        while (i <= j) {
            while (arr[i] < pivot) i++
            while (arr[j] > pivot) j--
            if (i <= j) {
                swap(arr, i, j)
                i++
                j--
            }
        }
        if (left < j) quicksort(arr, left, j)
        if (i < right) quicksort(arr, i, right)
    }

    function percentile(arr, n, p,    rank) {
        if (n == 0) return 0
        rank = int((p * n + 99) / 100)
        if (rank < 1) rank = 1
        if (rank > n) rank = n
        return arr[rank]
    }

    function print_group(label, arr, n,    i) {
        printf "\n=== %s ===\n", label
        if (n == 0) {
            print "N = 0 (no samples)"
            return
        }
        printf "N   = %d\n", n
        printf "min = %d\n", arr[1]
        printf "p50 = %d\n", percentile(arr, n, 50)
        printf "p95 = %d\n", percentile(arr, n, 95)
        printf "p99 = %d\n", percentile(arr, n, 99)
        print "raw (ascending):"
        for (i = 1; i <= n; i++) {
            printf "  %d\n", arr[i]
        }
    }

    {
        if ($0 !~ /latency_sample/) next
        a = ""
        b = ""
        for (i = 1; i <= NF; i++) {
            if ($i ~ /^edge_to_write_ns=/) {
                a = $i
                sub(/^edge_to_write_ns=/, "", a)
            } else if ($i ~ /^write_to_read_ns=/) {
                b = $i
                sub(/^write_to_read_ns=/, "", b)
            }
        }
        if (a !~ /^[0-9]+$/) next
        total++
        na++
        edge[na] = a + 0
        if (b ~ /^[0-9]+$/) {
            nb++
            wtr[nb] = b + 0
            nc++
            tot[nc] = (a + 0) + (b + 0)
        }
    }

    END {
        quicksort(edge, 1, na)
        quicksort(wtr, 1, nb)
        quicksort(tot, 1, nc)

        printf "Latency report (GB-6) - nearest-rank percentiles, nanoseconds\n"
        printf "source: %s\n", source
        printf "latency_sample lines parsed: %d\n", total

        print_group("(a) edge -> write-issued (edge_to_write_ns)", edge, na)
        print_group("(b) write -> read-returned (write_to_read_ns)", wtr, nb)
        print_group("(c) derived total = (a) + (b)", tot, nc)
    }
    '
}

input_label=stdin
if [ "$#" -eq 1 ] && [ "$1" != "-" ]; then
    if [ ! -r "$1" ]; then
        printf '%s: cannot read %s\n' "$prog" "$1" >&2
        exit 1
    fi
    input_label=$1
    report <"$1"
else
    report
fi
