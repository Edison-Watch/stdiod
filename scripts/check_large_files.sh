#!/usr/bin/env bash
#
# Enforce a line-count limit on source files. Shared by
# .github/workflows/large-files.yaml and prek.toml.
#   check_large_files.sh [file ...]   # check the given files
#   check_large_files.sh --all        # scan the whole tree
# Thresholds via LARGE_FILE_WARN_THRESHOLD / LARGE_FILE_ERROR_THRESHOLD.
# Exit 1 on errors, 0 on warnings-only or clean.
set -euo pipefail

WARN_THRESHOLD="${LARGE_FILE_WARN_THRESHOLD:-500}"
ERROR_THRESHOLD="${LARGE_FILE_ERROR_THRESHOLD:-800}"

SOURCE_EXTS=(rs)

declare -A _EXT_SET=()
for e in "${SOURCE_EXTS[@]}"; do _EXT_SET[".$e"]=1; done

EXCLUDE_PATH_RE='(^|/)(vendor|target|\.git)(/|$)'
GENERATED_RE='(^|/)(alembic[^/]*/versions|migrations)(/|$)'
EXCLUDE_NAME_RE='(.+_test\..+)$'

is_source_file() { local ext=".${1##*.}"; [ -n "${_EXT_SET[$ext]:-}" ]; }

is_excluded() {
  local f="$1" base
  echo "$f" | grep -qE "$EXCLUDE_PATH_RE" && return 0
  echo "$f" | grep -qE "$GENERATED_RE" && return 0
  base=$(basename "$f")
  echo "$base" | grep -qE "$EXCLUDE_NAME_RE" && return 0
  return 1
}

collect_all() {
  local fa=() first=1
  for e in "${SOURCE_EXTS[@]}"; do
    if [ "$first" = 1 ]; then fa+=( -name "*.$e" ); first=0; else fa+=( -o -name "*.$e" ); fi
  done
  find . -type f \( "${fa[@]}" \) \
    -not -path './.git/*' -not -path '*/node_modules/*' \
    -not -path '*/__pycache__/*' -not -path '*/.venv/*' -not -path '*/target/*' \
    | sed 's|^\./||'
}

files=()
if [ "${1:-}" = "--all" ]; then mapfile -t files < <(collect_all); else files=("$@"); fi

warnings=0; errors=0; warn_list=""; error_list=""
for file in "${files[@]}"; do
  [ -z "$file" ] && continue
  [ ! -f "$file" ] && continue
  is_source_file "$file" || continue
  is_excluded "$file" && continue
  lines=$(wc -l < "$file")
  if [ "$lines" -gt "$ERROR_THRESHOLD" ]; then
    errors=$((errors + 1))
    error_list="${error_list}| \`${file}\` | ${lines} | :x: exceeds ${ERROR_THRESHOLD} |\n"
  elif [ "$lines" -gt "$WARN_THRESHOLD" ]; then
    warnings=$((warnings + 1))
    warn_list="${warn_list}| \`${file}\` | ${lines} | :warning: exceeds ${WARN_THRESHOLD} |\n"
  fi
done

if [ -n "${GITHUB_STEP_SUMMARY:-}" ] && { [ "$errors" -gt 0 ] || [ "$warnings" -gt 0 ]; }; then
  {
    echo "## Large File Report"; echo ""
    echo "| File | Lines | Status |"; echo "|------|-------|--------|"
    [ "$errors" -gt 0 ] && printf '%b' "$error_list"
    [ "$warnings" -gt 0 ] && printf '%b' "$warn_list"
    echo ""; echo "**Thresholds:** warn at ${WARN_THRESHOLD} lines, error at ${ERROR_THRESHOLD} lines"
  } >> "$GITHUB_STEP_SUMMARY"
fi

format_list() { if command -v column >/dev/null 2>&1; then printf '%b' "$1" | column -t -s '|'; else printf '%b' "$1"; fi; }

if [ "$errors" -gt 0 ]; then echo "::error::${errors} file(s) exceed the ${ERROR_THRESHOLD}-line error threshold" >&2; format_list "$error_list" >&2; fi
if [ "$warnings" -gt 0 ]; then echo "::warning::${warnings} file(s) exceed the ${WARN_THRESHOLD}-line warning threshold" >&2; format_list "$warn_list" >&2; fi
if [ "$errors" -eq 0 ] && [ "$warnings" -eq 0 ]; then echo "All source files are within the ${WARN_THRESHOLD}-line limit."; fi

[ "$errors" -gt 0 ] && exit 1
exit 0
