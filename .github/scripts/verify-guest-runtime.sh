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
  x86_64-unknown-linux-musl)
    expected_machine="Advanced Micro Devices X86-64"
    maximum_page_size=4096
    ;;
  aarch64-unknown-linux-musl)
    expected_machine="AArch64"
    maximum_page_size=65536
    ;;
  *)
    echo "Unsupported guest runtime target: ${target}" >&2
    exit 64
    ;;
esac
if [[ "${elf_machine}" != "${expected_machine}" ]]; then
  echo "Guest runtime has machine ${elf_machine}, expected ${expected_machine}" >&2
  exit 1
fi
program_headers=$(readelf --wide --program-headers "${binary}")
if grep -q INTERP <<<"${program_headers}"; then
  echo "Guest runtime requires a program interpreter" >&2
  exit 1
fi
if readelf --dynamic "${binary}" | grep -q NEEDED; then
  echo "Guest runtime has a dynamic dependency" >&2
  exit 1
fi

# RELRO must end on a page boundary for every supported host page size. The
# first protected page must not also contain ordinary writable data.
writable_loads=()
relro_start=
relro_end=
while read -r kind offset address physical_address file_bytes memory_bytes flags; do
  case "${kind}" in
    LOAD)
      alignment=${flags##* }
      if (( alignment < maximum_page_size || alignment % maximum_page_size != 0 )); then
        echo "Guest runtime LOAD alignment is smaller than ${maximum_page_size} bytes" >&2
        exit 1
      fi
      if [[ "${flags}" == *W* ]]; then
        writable_loads+=("${address}:${memory_bytes}")
      fi
      ;;
    GNU_RELRO)
      if [[ -n "${relro_start}" ]]; then
        echo "Guest runtime has more than one RELRO segment" >&2
        exit 1
      fi
      relro_start=$((address))
      relro_end=$((address + memory_bytes))
      ;;
  esac
done <<<"${program_headers}"
if [[ -z "${relro_start}" ]]; then
  echo "Guest runtime has no RELRO segment" >&2
  exit 1
fi
if (( relro_end <= relro_start || relro_end % maximum_page_size != 0 )); then
  echo "Guest runtime RELRO end is not aligned to ${maximum_page_size} bytes" >&2
  exit 1
fi
protected_start=$((relro_start & ~(maximum_page_size - 1)))
for load in "${writable_loads[@]}"; do
  load_start=$((${load%:*}))
  load_end=$((load_start + ${load#*:}))
  if (( load_start < relro_start && load_end > protected_start )); then
    echo "Guest runtime RELRO page contains writable data outside RELRO" >&2
    exit 1
  fi
done

if [[ -n "${run_binary}" ]]; then
  if [[ "${run_binary}" != "--run" ]]; then
    echo "Unknown verification option: ${run_binary}" >&2
    exit 1
  fi
  "${binary}" --identify | cargo run --quiet --locked \
    -p lasper-guest-protocol --example verify_identity -- "${target}"
  if [[ "$("${binary}" --help)" != "Usage: lasper-guest-runtime --identify | --broker DIRECTORY" ]]; then
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
