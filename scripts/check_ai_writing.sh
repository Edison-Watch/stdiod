#!/usr/bin/env bash
# AI writing check: fail on em dashes or contrastive-parallelism constructions.
# Pure ripgrep/git-grep - no language runtime or build needed.
# POSIX-ERE-safe patterns (no lazy quantifiers, no (?:); apostrophes written as
# `.` so they match '/’ and dodge shell quoting) work in both rg and git grep -E.
set -uo pipefail
cd "$(dirname "$0")/.."

EM_DASH=$'\xe2\x80\x94'   # U+2014
SELF="scripts/check_ai_writing.sh"

CONTRA='not (just|only|merely|simply)[^.?!]{0,60} but'
CONTRA="$CONTRA"'|(it.s|that.s|this is) not [^.?!]{0,60}(it.s|that.s|they.re)'
CONTRA="$CONTRA"'|(isn.t|aren.t|wasn.t|weren.t) (just|only|merely|simply)'
CONTRA="$CONTRA"'|(isn.t|aren.t) (just )?about[^.?!]{0,60}it.s about'
CONTRA="$CONTRA"'|more than just'
CONTRA="$CONTRA"'|less about[^.?!]{0,60}more about'
CONTRA="$CONTRA"'|not [^.?!]{0,40}so much as'
CONTRA="$CONTRA"'|goes? beyond'   # noisiest; drop this line if it over-flags

if command -v rg >/dev/null 2>&1; then
  em=$(rg -n --hidden --glob '!.git' --glob "!$SELF" -e "$EM_DASH" . || true)
  contra=$(rg -ni --hidden --glob '!.git' --glob "!$SELF" -e "$CONTRA" . || true)
else
  # git grep scans tracked files only - target/ and other build output excluded for free.
  em=$(git grep -n -e "$EM_DASH" -- . ":(exclude)$SELF" || true)
  contra=$(git grep -niE -e "$CONTRA" -- . ":(exclude)$SELF" || true)
fi

fail=0
if [ -n "$em" ]; then echo "AI writing check failed: em dash (U+2014) detected"; echo "$em"; fail=1; fi
if [ -n "$contra" ]; then echo "AI writing check failed: contrastive parallelism ('not just X, but Y') detected"; echo "$contra"; fail=1; fi
[ "$fail" -eq 0 ] && echo "AI writing check passed."
exit "$fail"
