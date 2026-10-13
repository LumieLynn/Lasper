#!/usr/bin/env bash
set -euo pipefail

: "${GITHUB_REPOSITORY:?}"
: "${RELEASE_VERSION:?}"
: "${RELEASE_SHA:?}"

target=$(git rev-parse --verify "${RELEASE_SHA}^{commit}")
tag="v${RELEASE_VERSION}"
repository_url="https://github.com/${GITHUB_REPOSITORY}"
published_tags=$(
  gh api --paginate --slurp "repos/${GITHUB_REPOSITORY}/releases?per_page=100" |
    jq -r 'add // []
      | map(select(.draft == false))
      | sort_by(.published_at)
      | reverse
      | .[]
      | select(
          (.prerelease == true and (.tag_name | test("^v[0-9]+\\.[0-9]+\\.[0-9]+-dev\\.[0-9a-f]{7,40}$"))) or
          (.prerelease == false and (.tag_name | test("^v[0-9]+\\.[0-9]+\\.[0-9]+$")))
        )
      | .tag_name'
)

baseline_tag=
baseline_distance=
range="${target}"
while IFS= read -r candidate_tag; do
  [[ -n "${candidate_tag}" && "${candidate_tag}" != "${tag}" ]] || continue
  candidate=$(git rev-parse --verify "refs/tags/${candidate_tag}^{commit}" 2>/dev/null) || continue
  git merge-base --is-ancestor "${candidate}" "${target}" || continue
  distance=$(git rev-list --count "${candidate}..${target}")
  if [[ -z "${baseline_distance}" ]] || (( distance < baseline_distance )); then
    baseline_tag="${candidate_tag}"
    baseline_distance="${distance}"
    range="${candidate}..${target}"
  fi
done <<< "${published_tags}"

printf '# Lasper %s\n\n' "${RELEASE_VERSION}"
printf 'Development snapshot built from dev commit [`%s`](%s/commit/%s).\n\n' \
  "${target:0:7}" "${repository_url}" "${target}"
if [[ -n "${baseline_tag}" ]]; then
  printf 'New commits since [%s](%s/compare/%s...%s).\n\n' \
    "${baseline_tag}" "${repository_url}" "${baseline_tag}" "${tag}"
else
  printf 'Commits included in this first development snapshot.\n\n'
fi

if [[ "$(git rev-list --count --no-merges "${range}")" == 0 ]]; then
  printf 'No new commits.\n'
else
  git log --reverse --topo-order --no-merges --abbrev=7 \
    --format="### [\`%h\`](${repository_url}/commit/%H) %s%n%n%b" "${range}"
fi
