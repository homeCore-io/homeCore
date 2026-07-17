//! Dev / registry-publishing helper: sign a registry `index.json` with an
//! ed25519 key and emit the base64 public key.
//!
//!   cargo run -p hc-api --example registry_sign -- <index.json> [seed_byte]
//!
//! Writes `<index.json>.sig` (base64 of the detached 64-byte signature) next to
//! the file and prints the base64 public key on stdout. The seed_byte form is a
//! DEV convenience (deterministic key); a real registry signs with a secret key
//! kept out of the repo.

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .expect("usage: registry_sign <index.json> [seed_byte]");
    let seed: u8 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7);

    let sk = SigningKey::from_bytes(&[seed; 32]);
    let bytes = std::fs::read(path).expect("reading index");
    let sig = sk.sign(&bytes).to_bytes();
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig);
    std::fs::write(format!("{path}.sig"), &sig_b64).expect("writing .sig");

    let pk = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
    println!("{pk}");
}
