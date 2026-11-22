//! argon2id hashing for realm user passwords and client secrets (design
//! §8.1, §13.1 `client-confidential-symmetric`). Same OWASP 2024 parameters
//! as the console (`$argon2id$v=19$m=19456,t=2,p=1$…`).
//!
//! Verification is the expensive part of a login or a `client_secret_*`
//! authentication, so callers bound concurrency (a semaphore in the server)
//! and always verify — against [`dummy_hash`] when the account or client is
//! unknown — so timing does not reveal which exist.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

/// argon2id memory cost, KiB (OWASP 2024: 19 MiB).
pub const M_COST: u32 = 19_456;
/// argon2id iterations.
pub const T_COST: u32 = 2;
/// argon2id parallelism.
pub const P_COST: u32 = 1;
/// Longest accepted password or secret (argon2 cost is per byte).
pub const MAX_LENGTH: usize = 256;

fn argon2() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(M_COST, T_COST, P_COST, None).expect("static argon2 parameters are valid"),
    )
}

/// Hashes `secret` with a fresh random salt (PHC string).
pub fn hash(secret: &str) -> String {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    argon2()
        .hash_password(secret.as_bytes(), &salt)
        .expect("argon2id hashing with valid parameters cannot fail")
        .to_string()
}

/// Whether `secret` matches the PHC `phc`. Only argon2id hashes are accepted;
/// a malformed hash, another algorithm or an over-long input never matches.
pub fn verify(secret: &str, phc: &str) -> bool {
    if secret.len() > MAX_LENGTH || !phc.starts_with("$argon2id$") {
        return false;
    }
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A hash to verify against when the user or client does not exist.
pub fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hash("dummy secret for timing equalization"))
}

/// A new client secret: 256 bits from the OS CSPRNG, base64url (43 chars).
/// Shown once; only [`hash`] of it is stored.
pub fn generate_client_secret() -> String {
    crate::util::random_b64(32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash("correct horse battery staple");
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify("correct horse battery staple", &h));
        assert!(!verify("correct horse battery stapl", &h));
        assert!(!verify("x", "not a hash"));
        assert!(!verify(&"a".repeat(300), &h));
        assert!(!verify("anything", dummy_hash()));
        let s = generate_client_secret();
        assert_eq!(s.len(), 43);
        assert_ne!(s, generate_client_secret());
    }

    #[test]
    fn non_argon2id_hashes_refused() {
        // argon2i PHC strings verify with Argon2::default(); we refuse them.
        let h = hash("pw").replacen("$argon2id$", "$argon2i$", 1);
        assert!(!verify("pw", &h));
    }
}
