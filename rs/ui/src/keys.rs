#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Prompt,
    Approve,
    Rollback,
    Quit,
    Fold(usize),
    Other,
}

pub fn parse(byte: u8) -> Key {
    match byte {
        b'p' | b'P' => Key::Prompt,
        b'a' | b'A' => Key::Approve,
        b'r' | b'R' => Key::Rollback,
        b'q' | b'Q' => Key::Quit,
        b'0'..=b'9' => Key::Fold((byte - b'0') as usize),
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_action_keys() {
        assert_eq!(parse(b'p'), Key::Prompt);
        assert_eq!(parse(b'P'), Key::Prompt);
        assert_eq!(parse(b'a'), Key::Approve);
        assert_eq!(parse(b'r'), Key::Rollback);
        assert_eq!(parse(b'q'), Key::Quit);
    }

    #[test]
    fn recognizes_digits_as_fold() {
        assert_eq!(parse(b'0'), Key::Fold(0));
        assert_eq!(parse(b'9'), Key::Fold(9));
    }

    #[test]
    fn everything_else_is_other() {
        assert_eq!(parse(b'z'), Key::Other);
        assert_eq!(parse(b' '), Key::Other);
    }
}
