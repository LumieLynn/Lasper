use super::*;

#[test]
fn proc_stat_parent_handles_spaces_and_parentheses_in_comm() {
    assert_eq!(stat_parent(b"20 (an odd ) name) S 17 20 20 0\n"), Some(17));
    assert_eq!(stat_parent(b"20 (missing state)\n"), None);
}
