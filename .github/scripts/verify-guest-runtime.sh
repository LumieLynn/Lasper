#!/usr/bin/env bash

set -euo pipefail
export LC_ALL=C

target=${1:?usage: verify-guest-runtime.sh TARGET [--run]}
run_binary=${2:-}
binary="target/${target}/guest-runtime/lasper-guest-runtime"

if [[ ! -x "${binary}" ]]; then
  echo "Guest runtime was not built at ${binary}" >&2
  exit 1
fi

elf_type=$(readelf --file-header "${binary}" | awk '$1 == "Type:" { print $2 }')
if [[ "${elf_type}" != "DYN" ]]; then
  echo "Guest runtime is not a position-independent executable" >&2
  exit 1
fi
elf_machine=$(readelf --file-header "${binary}" | sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p')
case "${target}" in
  x86_64-unknown-linux-musl) expected_machine="Advanced Micro Devices X86-64" ;;
  aarch64-unknown-linux-musl) expected_machine="AArch64" ;;
  *)
    echo "Unsupported guest runtime target: ${target}" >&2
    exit 64
    ;;
esac
if [[ "${elf_machine}" != "${expected_machine}" ]]; then
  echo "Guest runtime has machine ${elf_machine}, expected ${expected_machine}" >&2
  exit 1
fi
if readelf --program-headers "${binary}" | grep -q INTERP; then
  echo "Guest runtime requires a program interpreter" >&2
  exit 1
fi
if readelf --dynamic "${binary}" | grep -q NEEDED; then
  echo "Guest runtime has a dynamic dependency" >&2
  exit 1
fi

if [[ -n "${run_binary}" ]]; then
  if [[ "${run_binary}" != "--run" ]]; then
    echo "Unknown verification option: ${run_binary}" >&2
    exit 1
  fi
  "${binary}" --identify | cargo run --quiet --locked \
    -p lasper-guest-protocol --example verify_identity -- "${target}"
  if [[ "$("${binary}" --help)" != "Usage: lasper-guest-runtime --identify" ]]; then
    echo "Guest runtime returned an unexpected help response" >&2
    exit 1
  fi
  if "${binary}" --invalid >/dev/null 2>&1; then
    status=0
  else
    status=$?
  fi
  if [[ ${status} -eq 0 ]]; then
    echo "Guest runtime accepted an unknown argument" >&2
    exit 1
  fi
  if [[ ${status} -ne 64 ]]; then
    echo "Guest runtime returned ${status} for an unknown argument, expected 64" >&2
    exit 1
  fi
fi
