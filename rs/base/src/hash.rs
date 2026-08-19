use sha2::{Digest, Sha256};

pub fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn sha256(body: impl AsRef<[u8]>) -> String {
    hex(Sha256::digest(body.as_ref()))
}

/// The first `bytes` bytes of sha256(body), as hex: the short ids a home, a
/// payload and an episode's state are keyed by.
pub fn short(body: impl AsRef<[u8]>, bytes: usize) -> String {
    hex(&Sha256::digest(body.as_ref())[..bytes])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helpers_agree_with_the_hand_written_loops() {
        let sum = Sha256::digest(b"tenon");
        let by_hand: String = sum.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(sha256("tenon"), by_hand);
        assert_eq!(short("tenon", 6), by_hand[..12]);
        assert_eq!(hex([0u8, 15, 255]), "000fff");
    }
}
