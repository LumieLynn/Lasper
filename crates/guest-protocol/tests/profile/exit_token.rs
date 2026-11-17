use super::*;

impl ExitFrame {
    // Independent fixture codecs need the token without exposing it in the
    // public frame API or decoding an intermediate V1 payload.
    pub(crate) fn fixture_token(&self) -> (BrokerId, PreparationId) {
        (self.broker_id, self.preparation_id)
    }
}
