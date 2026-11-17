use super::*;

#[test]
fn maps_multiple_extents_without_assuming_one_shift() {
    let map = IdMap::parse("0 1437401088 65536\n70000 200000 100\n").unwrap();
    assert_eq!(map.map(1000), Some(1_437_402_088));
    assert_eq!(map.map(70_042), Some(200_042));
    assert_eq!(map.map(65_536), None);
}

#[test]
fn invisible_and_ambiguous_extents_cannot_authorize_an_id() {
    let map = IdMap::parse("0 4294967295 65536\n70000 200000 100\n").unwrap();
    assert_eq!(map.map(1000), None);
    assert_eq!(map.map(70_042), Some(200_042));
    assert_eq!(IdMap::parse("0 1000 10\n5 2000 10\n").unwrap().map(7), None);
    assert_eq!(
        IdMap::parse("0 0 4294967295\n").unwrap().map(u32::MAX),
        None
    );
}

#[test]
fn rejects_invalid_and_unbounded_input() {
    for text in [
        "",
        "0 1000",
        "0 uid 1",
        "0 1000 0",
        "0 1000 1 2",
        "4294967295 0 2",
        "0 4294967294 3",
        "0 4294967296 1",
    ] {
        assert!(IdMap::parse(text).is_err(), "{text}");
    }
    let excessive = std::iter::repeat_n("0 0 1", MAX_ID_MAP_EXTENTS + 1)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(IdMap::parse(&excessive).is_err());
    assert!(IdMap::read([0xff].as_slice()).is_err());
    let oversized = vec![b' '; MAX_ID_MAP_BYTES as usize + 1];
    assert!(IdMap::parse(std::str::from_utf8(&oversized).unwrap()).is_err());
    assert!(IdMap::read(oversized.as_slice()).is_err());
}
