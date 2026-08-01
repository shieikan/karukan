#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <explicit-corpus.tsv> <explicit-dict.bin>" >&2
    exit 2
fi

corpus_path=$1
dict_path=$2
expected_size=205844926
expected_hash=d85fed3c7e408e67f4a5dbe2314338366ed6183e0498d2726bc5a4dcaf5bb83b

if [[ ! -f "$corpus_path" || ! -f "$dict_path" ]]; then
    echo "dictionary quality: corpus or dictionary path is missing" >&2
    exit 20
fi

actual_size=$(wc -c < "$dict_path" | tr -d '[:space:]')
if [[ "$actual_size" != "$expected_size" ]]; then
    echo "dictionary quality: dictionary size mismatch (expected $expected_size, got $actual_size)" >&2
    exit 21
fi

actual_hash=$(shasum -a 256 "$dict_path" | awk '{print $1}')
if [[ "$actual_hash" != "$expected_hash" ]]; then
    echo "dictionary quality: dictionary hash mismatch" >&2
    exit 22
fi

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

while IFS=$'\t' read -r reading expected_surface max_rank extra; do
    [[ -z "$reading" || "$reading" == \#* ]] && continue
    if [[ -z "$expected_surface" || -z "$max_rank" || -n "$extra" ]]; then
        echo "dictionary quality: corpus row must contain exactly reading, surface, max_rank (reading=$reading)" >&2
        exit 2
    fi
    if [[ ! "$max_rank" =~ ^[1-9][0-9]*$ ]]; then
        echo "dictionary quality: max_rank must be a positive integer (reading=$reading, max_rank=$max_rank)" >&2
        exit 2
    fi

    query_output="$tmp_dir/query.txt"
    if ! cargo run --quiet --package karukan-cli --bin karukan-dict -- \
        view --query "$reading" "$dict_path" >"$query_output" 2>"$tmp_dir/query.err"; then
        echo "dictionary quality: dictionary lookup failed for reading=$reading" >&2
        exit 2
    fi

    rank=$(awk -F '\t' -v expected="$expected_surface" '
        NF >= 3 {
            candidate_rank++
            if ($2 == expected) {
                print candidate_rank
                exit
            }
        }
    ' "$query_output")
    if [[ -z "$rank" ]]; then
        rank=absent
    fi

    echo "reading=$reading surface=$expected_surface rank=$rank max_rank=$max_rank dict=$dict_path"
    if [[ "$rank" == absent ]]; then
        exit 30
    fi

    if (( rank > max_rank )); then
        exit 31
    fi
done < "$corpus_path"

printf 'dictionary quality: verified corpus=%s dict=%s\n' "$corpus_path" "$dict_path"
