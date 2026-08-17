#!/usr/bin/env bash
#
# Verify that every relative link in every shipping Markdown file resolves to a
# file that will actually be published.
#
# WHY THIS EXISTS
# ---------------
# This repository publishes by taking a fresh `git init` + `git add -A` of the
# working tree, which makes `.gitignore` the sole authority on what ships. That
# creates a failure mode no ordinary link checker catches: a linked file can be
# present on disk, open fine in an editor, and still be absent from the
# published repo because a `.gitignore` rule excludes it.
#
# That is not hypothetical. Before this script existed, every documentation link
# in README.md pointed into `docs/`, which is gitignored — so all five would have
# 404'd on the day the repo went public, while looking correct to anyone reading
# locally.
#
# So this script deliberately checks link targets against the *shipping set*
# rather than against the filesystem. A target that exists locally but is
# excluded from publication is reported as broken, because for a reader it is.
#
# Usage:  dev/check-doc-links.sh
# Exit:   0 = every link resolves, 1 = at least one broken link (details on stderr)

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# --- Build the shipping set -------------------------------------------------
# Candidates: tracked files plus untracked-but-not-ignored files. This is what a
# fresh `git add -A` would consider.
git ls-files --cached --others --exclude-standard | sort -u > "$work/candidates"

# Tracked files can still be ignored (a rule added after the file was committed
# does not untrack it), and those will NOT survive a fresh init. Filter them out
# so the shipping set matches what actually gets published.
#
# `--no-index` is load-bearing, not decoration. Without it, `git check-ignore`
# consults the index and refuses to report a *tracked* file as ignored, on the
# reasoning that the file is already committed so the rule is moot. That
# reasoning is wrong for this repo: publication is a fresh `git init`, where no
# index exists and the rule decides everything. Dropping `--no-index` silently
# admits every tracked-but-ignored file into the shipping set and makes this
# whole check report a pass it did not earn.
: > "$work/ignored"
if [ -s "$work/candidates" ]; then
  git check-ignore --no-index --stdin < "$work/candidates" 2>/dev/null \
    | sort -u > "$work/ignored" || true
fi

comm -23 "$work/candidates" "$work/ignored" > "$work/ship"

ship_count=$(wc -l < "$work/ship" | tr -d ' ')
grep -E '\.md$' "$work/ship" > "$work/ship_md" || true
md_count=$(wc -l < "$work/ship_md" | tr -d ' ')

echo "Shipping set: ${ship_count} files, ${md_count} markdown."

# --- Normalize a path: resolve . and .. segments lexically -------------------
normalize() {
  printf '%s\n' "$1" | awk -F/ '{
    n = 0
    for (i = 1; i <= NF; i++) {
      if ($i == "" || $i == ".") continue
      if ($i == "..") { if (n > 0) n--; continue }
      stack[++n] = $i
    }
    out = ""
    for (i = 1; i <= n; i++) out = (i == 1 ? stack[i] : out "/" stack[i])
    print out
  }'
}

broken=0
checked=0

while IFS= read -r md; do
  [ -n "$md" ] || continue
  dir="$(dirname "$md")"

  # Extract link targets from [text](target) and ![alt](target).
  # Strips an optional "title", tolerates <angle brackets>, ignores nothing else.
  targets="$(sed -E 's/\]\(/\n\](/g' "$md" \
    | grep -oE '^\]\(<?[^)>"[:space:]]+' \
    | sed -E 's/^\]\(<?//' || true)"

  [ -n "$targets" ] || continue

  while IFS= read -r target; do
    [ -n "$target" ] || continue

    # Skip absolute URLs, protocol-relative URLs, mailto:, and pure anchors.
    case "$target" in
      http://*|https://*|//*|mailto:*|\#*|tel:*) continue ;;
    esac

    # Drop any #fragment; keep the path part.
    path="${target%%#*}"
    [ -n "$path" ] || continue

    checked=$((checked + 1))

    if [ "${path#/}" != "$path" ]; then
      # Repo-absolute link (leading slash) — resolve from the repo root.
      resolved="$(normalize "${path#/}")"
    else
      resolved="$(normalize "$dir/$path")"
    fi

    # A target is fine if it is a file in the shipping set, or a directory
    # prefix of at least one shipping file.
    if grep -Fxq "$resolved" "$work/ship"; then
      continue
    fi
    if grep -Fq "${resolved%/}/" "$work/ship"; then
      continue
    fi

    if [ -e "$resolved" ]; then
      reason="exists on disk but is NOT in the shipping set (gitignored or untracked)"
    else
      reason="does not exist"
    fi
    echo "BROKEN  $md  ->  $target" >&2
    echo "        resolves to '$resolved', which $reason" >&2
    broken=$((broken + 1))
  done <<EOF
$targets
EOF
done < "$work/ship_md"

echo "Checked ${checked} relative links across ${md_count} shipping markdown files."

if [ "$broken" -gt 0 ]; then
  echo "FAIL: ${broken} broken link(s)." >&2
  exit 1
fi

echo "OK: every relative link resolves to a file that will ship."
