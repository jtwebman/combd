#!/bin/sh
# Build the docs into app/static (mdbook), with the same pages as plain
# markdown for tools that read docs (llms.txt, llms-full.txt, and each page
# at its name with .md), and a .gz beside each text file (and a .br, where
# brotli is installed) for browsers that take them. Then:
# combd deploy app <org> https://arena.combd.dev:9443 (COMBD_TOKEN).
set -e
cd "$(dirname "$0")"
mdbook build
rm -rf app/static && cp -R book app/static
# The pages, in the book's order.
pages=$(grep -oE '\([a-z0-9-]+\.md\)' src/SUMMARY.md | tr -d '()')
for p in $pages; do cp "src/$p" "app/static/$p"; done
{
    echo "# combd"
    echo
    echo "> Durable Objects on your own machines. Each object is a Wasm cell with its own SQLite. Run one node, or a fleet over an S3 or R2 bucket. One binary."
    echo
    echo "Each page below is plain markdown. llms-full.txt is all of them in one file. To set up a fleet, follow \"Set up a fleet, step by step\" in fleet.md."
    echo
    echo "## Pages"
    echo
    for p in $pages; do
        title=$(grep -m1 '^# ' "src/$p" | sed 's/^# //')
        echo "- [$title](https://docs.combd.dev/$p)"
    done
    echo "- [Everything, in one file](https://docs.combd.dev/llms-full.txt)"
} > app/static/llms.txt
for p in $pages; do cat "src/$p"; printf '\n\n'; done > app/static/llms-full.txt
find app/static -type f \( -name '*.html' -o -name '*.css' -o -name '*.js' -o -name '*.json' -o -name '*.svg' -o -name '*.txt' -o -name '*.md' \) | while read -r f; do
    gzip -9 -k -n "$f"
    if command -v brotli >/dev/null; then brotli -q 11 -k "$f"; fi
done
echo "docs: app/static ($(find app/static -type f | wc -l | tr -d ' ') files)"
