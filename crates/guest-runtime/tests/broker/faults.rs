pub fn install(fault: &str) {
    let number = match fault {
        "fork" => libc::SYS_clone,
        "pidfd" => libc::SYS_pidfd_open,
        "bind" => libc::SYS_bind,
        _ => panic!("unknown broker fault"),
    };
    let mut filter = vec![
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as _,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as _,
            jt: 0,
            jf: if fault == "pidfd" { 3 } else { 1 },
            k: number as _,
        },
    ];
    if fault == "pidfd" {
        // Readiness opens our own pidfd; fail only the guardian assignment.
        filter.extend([
            libc::sock_filter {
                code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as _,
                jt: 0,
                jf: 0,
                k: 16,
            },
            libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as _,
                jt: 1,
                jf: 0,
                k: std::process::id(),
            },
        ]);
    }
    filter.extend([
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as _,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | libc::EMFILE as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as _,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ]);
    let program = libc::sock_fprog {
        len: filter.len() as _,
        filter: filter.as_mut_ptr(),
    };
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) },
        0
    );
}
