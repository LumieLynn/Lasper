// Quick test to verify url::Url::parse behavior with docker:// URLs
// Run with: cargo eval or similar
fn main() {
    // Simulating what url::Url::parse does
    let tests = vec![
        "docker://ubuntu:resolute",
        "docker://ubuntu:latest",
        "docker://ubuntu",
        "docker://registry.io/image:tag",
    ];
    for t in &tests {
        match url::Url::parse(t) {
            Ok(u) => println!("OK: {} -> host={:?}, port={:?}, path={}", t, u.host_str(), u.port(), u.path()),
            Err(e) => println!("ERR: {} -> {}", t, e),
        }
    }
}
