//! P10 G4: `sshsig` verification. An arbitrary armored signature over an
//! arbitrary message never panics and never verifies against a key that
//! did not make it.
#![no_main]
use contextdb_team::sshsig;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let key = sshsig::public_key(&[9; 32]);
    let (sig, msg) = data.split_at(data.len() / 2);
    let armored = String::from_utf8_lossy(sig);
    assert!(sshsig::verify(&armored, "git", msg, &[key]).is_err());
    let _ = sshsig::PublicKey::parse(&String::from_utf8_lossy(msg));
});
