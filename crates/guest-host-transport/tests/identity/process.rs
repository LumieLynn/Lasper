use super::*;

#[test]
fn shared_user_namespace_keeps_ids_in_the_readers_namespace() {
    let parent_map = IdMap::parse("0 100000 65536\n").unwrap();
    assert_eq!(mapped_id(1000, None), Some(1000));
    assert_eq!(mapped_id(1000, Some(&parent_map)), Some(101000));
    assert_eq!(mapped_id(u32::MAX, None), None);
    assert_eq!(mapped_id(65536, Some(&parent_map)), None);
}

#[test]
fn status_accepts_non_utf8_names_but_requires_complete_final_credentials() {
    let mut status = b"Name:\t\xffworker\nState:\tS (sleeping)\nTgid:\t42\n".to_vec();
    status.extend_from_slice(b"Uid:\t1000 1000 1000 1000\nGid:\t2000 2000 2000 2000\n");
    check_status(&status, 42, 1000, 2000).unwrap();
    for field in [
        "Uid:\t1000 0 1000 1000",
        "Gid:\t2000 2000 2000",
        "Tgid:\t43",
        "State:\tZ (zombie)",
    ] {
        let text = "State:\tS (sleeping)\nTgid:\t42\nUid:\t1000 1000 1000 1000\nGid:\t2000 2000 2000 2000\n";
        let key = field.split(':').next().unwrap();
        let text = text
            .lines()
            .map(|line| {
                if line.starts_with(&format!("{key}:")) {
                    field
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            check_status(text.as_bytes(), 42, 1000, 2000).is_err(),
            "{field}"
        );
    }
}
