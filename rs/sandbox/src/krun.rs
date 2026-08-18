use crate::Sandbox;

pub fn probe() -> Result<Box<dyn Sandbox>, String> {
    if std::path::Path::new("/dev/kvm").exists() {
        Err("krun backend arrives in P3.6".to_string())
    } else {
        Err("/dev/kvm absent".to_string())
    }
}
