# Shared by the fixed X11 and Wayland probes. Path arguments are never code.

probe_uint() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

probe_failure() {
    printf '%s\n' "$probe_magic" "ERROR=$1" 'RESULT=ERROR'
}

probe_guest_identity() {
    probe_euid=
    probe_egid=
    while read -r probe_key probe_real probe_effective probe_rest; do
        case "$probe_key" in
            Uid:) probe_euid=$probe_effective ;;
            Gid:) probe_egid=$probe_effective ;;
        esac
    done < /proc/self/status || return 1
    probe_uint "$probe_euid" && probe_uint "$probe_egid"
}

# One successful stat supplies both the file type and identity. A failed stat
# cannot distinguish ENOENT from EACCES through portable shell exit status.
# Report that uncertainty instead of claiming the path does not exist.
probe_socket() {
    if probe_metadata=$(LC_ALL=C stat -Lc '%d:%i:%f' -- "$2" 2>/dev/null); then
        probe_device=${probe_metadata%%:*}
        probe_remaining=${probe_metadata#*:}
        probe_inode=${probe_remaining%%:*}
        probe_mode=${probe_remaining#*:}
        case "$probe_mode" in
            ''|*[!0-9a-fA-F]*) probe_mode= ;;
        esac
        if probe_uint "$probe_device" && probe_uint "$probe_inode" && [ -n "$probe_mode" ]; then
            if [ "$((0x$probe_mode & 0170000))" -ne 49152 ]; then
                printf '%s\n' "$1=NOT_SOCKET"
            else
                if [ -w "$2" ]; then
                    probe_access=ACCESSIBLE
                else
                    probe_access=DENIED
                fi
                printf '%s\n' "$1=$probe_access" "$3=$probe_device" "$4=$probe_inode"
            fi
            return 0
        fi
    fi
    printf '%s\n' "$1=UNAVAILABLE"
}
