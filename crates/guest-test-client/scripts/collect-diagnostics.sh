#!/usr/bin/env bash
# Read-only host diagnostics; deliberately does not enter guest namespaces.
set -u
if [[ $# -ne 2 ]]; then
    echo "Usage: sudo bash $0 MACHINE OUTPUT" >&2
    exit 64
fi
machine=$1
output=$2
if [[ $EUID -ne 0 ]]; then
    echo "Run as host root so procfs and the endpoint metadata can be read." >&2
    exit 1
fi
if [[ ! $machine =~ ^[a-zA-Z0-9_.-]+$ ]]; then
    echo "Invalid machine name" >&2
    exit 64
fi
# Refuse to overwrite an earlier experiment record.
set -o noclobber
exec >"$output" 2>&1 || exit 1
date -Is
uname -a
systemd-nspawn --version
machinectl show "$machine" -p Name -p Leader -p Unit -p RootDirectory
leader=$(machinectl show "$machine" -p Leader --value)
unit=$(machinectl show "$machine" -p Unit --value)
if [[ ! $leader =~ ^[1-9][0-9]*$ ]]; then
    echo "No live registered leader; startup and host journal remain relevant."
    journalctl --no-pager -b -u systemd-machined -n 100
    exit 1
fi
object=$(busctl --system call org.freedesktop.machine1 /org/freedesktop/machine1 \
    org.freedesktop.machine1.Manager GetMachine s "$machine")
object=${object#*\"}
object=${object%\"*}
if [[ $object == /org/freedesktop/machine1/machine/* ]]; then
    busctl --system get-property org.freedesktop.machine1 "$object" \
        org.freedesktop.machine1.Machine LeaderPIDFDId
fi
systemctl show -p ControlGroup -p ActiveState -p SubState -- "$unit"
cat "/proc/$leader/status"
cat "/proc/$leader/cgroup"
cat "/proc/$leader/uid_map" "/proc/$leader/gid_map"
readlink "/proc/$leader/exe" "/proc/$leader/root" "/proc/$leader/cwd"
ls -l "/proc/$leader/ns"
printf 'leader cmdline: '
tr '\000' ' ' < "/proc/$leader/cmdline"
printf '\n'
ps -e -o pid=,ppid=,uid=,gid=,stat=,args= --forest |
    awk -v leader="$leader" '$1 == leader || ($2 in selected) { selected[$1] = 1; print }'
stat -L -- "/proc/$leader/root/var/opt/lasper" \
    "/proc/$leader/root/var/opt/lasper/guest-exec.sock" \
    "/proc/$leader/root/var/opt/lasper/broker.lock"
ss -xlpn | awk 'NR == 1 || index($0, "guest-exec.sock")'
journalctl --no-pager -b -u "$unit" -u systemd-machined -n 150
