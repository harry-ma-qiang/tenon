use std::io::Read;

pub fn generate() -> String {
    let mut bytes = [0u8; 32];
    let read_urandom =
        std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if read_urandom.is_err() {
        fallback(&mut bytes);
    }
    crate::hash::hex(bytes)
}

fn fallback(bytes: &mut [u8; 32]) {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128);
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = ((seed >> ((index % 16) * 8)) & 0xff) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_64_hex_chars_and_varies() {
        let a = generate();
        let b = generate();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
