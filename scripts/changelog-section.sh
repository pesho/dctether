#!/bin/sh
# Print the CHANGELOG.md section for one version, as release notes: its
# entries, then a link to the version's diff. Fails if there is no such
# section, so a release can't go out without a changelog entry.
#
#   scripts/changelog-section.sh 0.2.0 [CHANGELOG.md]
set -eu
version=$1
file=${2:-"$(dirname "$0")/../CHANGELOG.md"}
awk -v v="$version" '
  BEGIN { head = "## [" v "]" }
  index($0, "## [") == 1 { on = (index($0, head) == 1); if (on) found = 1; next }
  /^\[[^]]+\]: / { if (index($0, "[" v "]: ") == 1) link = substr($0, length(v) + 5); next }
  on { body = body $0 "\n" }
  END {
    if (!found) { print "no section for " v " in the changelog" > "/dev/stderr"; exit 1 }
    sub(/^\n+/, "", body); sub(/\n+$/, "\n", body)
    if (body == "" || body == "\n") { print "the " v " section of the changelog is empty" > "/dev/stderr"; exit 1 }
    printf "%s", body
    if (index(link, "/compare/")) printf "\n**Full diff:** %s\n", link
  }' "$file"
