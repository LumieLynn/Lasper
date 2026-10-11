//! Network-shaped values shared by runtime enumeration and systemd property
//! rendering.

/// Formats one address returned by machined's `(iay)` representation.
pub(crate) fn format_ip_address(family: i32, data: &[u8]) -> String {
    match family {
        libc::AF_INET => <[u8; 4]>::try_from(data)
            .map(std::net::Ipv4Addr::from)
            .map(|address| address.to_string())
            .unwrap_or_default(),
        libc::AF_INET6 => <[u8; 16]>::try_from(data)
            .map(std::net::Ipv6Addr::from)
            .map(|address| address.to_string())
            .unwrap_or_default(),
        _ => format!("[{} bytes]", data.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ipv4_and_ipv6_addresses() {
        assert_eq!(
            format_ip_address(libc::AF_INET, &[192, 168, 1, 1]),
            "192.168.1.1"
        );
        let data = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        assert_eq!(format_ip_address(libc::AF_INET6, &data), "2001:db8::1");
    }

    #[test]
    fn rejects_malformed_and_unknown_address_payloads() {
        assert_eq!(format_ip_address(libc::AF_INET, &[192, 168, 1]), "");
        assert_eq!(format_ip_address(libc::AF_INET6, &[0x20, 0x01]), "");
        assert_eq!(format_ip_address(99, &[1, 2, 3]), "[3 bytes]");
    }
}
