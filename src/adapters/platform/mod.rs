pub mod gpu;
pub mod network;
pub(crate) mod notifications;
pub mod nvidia;
pub mod wayland;
pub mod x11;

pub(crate) fn invoking_uid() -> u32 {
    if uzers::get_current_uid() == 0 {
        if let Ok(uid) = std::env::var("SUDO_UID") {
            if let Ok(uid) = uid.parse() {
                return uid;
            }
        }
    }
    uzers::get_current_uid()
}
