
```compile_fail,E0624
use lasper_guest_protocol::{protocol_profiles, StartFrame};
fn encode(frame: &StartFrame, output: &mut [u8]) {
    protocol_profiles()[0].encode_start(frame, output).unwrap();
}
```
