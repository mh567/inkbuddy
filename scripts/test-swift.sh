#!/bin/sh
# 编译并运行 Swift 侧测试。测试结果即进程退出码。
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$(mktemp -t inkbuddy-manifest-tests)"
trap 'rm -f "$OUT"' EXIT

swiftc "$ROOT/app/Sources/ManifestEditing.swift" \
    "$ROOT/app/Tests/ManifestEditingTests.swift" \
    -o "$OUT"
"$OUT"

set --
for source in "$ROOT"/app/Sources/*.swift; do
    [ "${source##*/}" = main.swift ] || set -- "$@" "$source"
done
swiftc "$@" "$ROOT/app/Tests/CanvasRatioTests.swift" -o "$OUT"
"$OUT"
