//! Local account passwords: argon2id hashing and the password policy.
//!
//! Hashes are PHC strings with the OWASP 2024 parameters
//! (`$argon2id$v=19$m=19456,t=2,p=1$…`). The policy refuses passwords that
//! are shorter than the configured minimum (default 12, never below 8), equal
//! to the user name, a single repeated character, or on the list of the
//! 10,000 most common passwords shipped in the binary.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

/// argon2id memory cost, KiB (OWASP 2024: 19 MiB).
pub const M_COST: u32 = 19_456;
/// argon2id iterations.
pub const T_COST: u32 = 2;
/// argon2id parallelism.
pub const P_COST: u32 = 1;
/// The floor under any configured minimum length.
pub const MIN_LENGTH_FLOOR: u8 = 8;
/// Upper bound on accepted password length (argon2 cost is per byte; a
/// megabyte "password" would be a cheap denial of service).
pub const MAX_LENGTH: usize = 256;

static BREACHED: &str = include_str!("breached.txt");

fn argon2() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(M_COST, T_COST, P_COST, None).expect("static argon2 parameters are valid"),
    )
}

/// Hashes `password` with a fresh random salt.
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    argon2()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2id hashing with valid parameters cannot fail")
        .to_string()
}

/// Whether `password` matches the PHC `hash`. A malformed hash never matches.
pub fn verify_password(password: &str, hash: &str) -> bool {
    if password.len() > MAX_LENGTH {
        return false;
    }
    match PasswordHash::new(hash) {
        // The parameters come from the hash itself, so hashes made with older
        // parameters keep verifying.
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A hash to verify against when the account does not exist, so a missing
/// user costs the same time as a wrong password (no user enumeration by
/// timing).
pub fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hash_password("dummy password for timing equalization"))
}

/// Why a new password was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyViolation {
    /// Shorter than the minimum.
    TooShort(usize),
    /// Longer than [`MAX_LENGTH`].
    TooLong,
    /// Equal to (or contains) the user name.
    ContainsUsername,
    /// One character repeated.
    Repetitive,
    /// On the common-passwords list.
    Common,
}

impl PolicyViolation {
    /// Fluent message id (`locales/*/console-auth.ftl`).
    pub fn message_id(&self) -> &'static str {
        match self {
            Self::TooShort(_) => "console-password-too-short",
            Self::TooLong => "console-password-too-long",
            Self::ContainsUsername => "console-password-username",
            Self::Repetitive => "console-password-repetitive",
            Self::Common => "console-password-common",
        }
    }
}

impl std::fmt::Display for PolicyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort(n) => write!(f, "the password must have at least {n} characters"),
            Self::TooLong => write!(f, "the password must have at most {MAX_LENGTH} characters"),
            Self::ContainsUsername => f.write_str("the password must not contain the user name"),
            Self::Repetitive => f.write_str("the password must not repeat a single character"),
            Self::Common => f.write_str("the password is on the list of common passwords"),
        }
    }
}

/// Checks a new password against the policy.
pub fn check_policy(password: &str, username: &str, min_length: u8) -> Result<(), PolicyViolation> {
    let min = usize::from(min_length.max(MIN_LENGTH_FLOOR));
    let chars = password.chars().count();
    if chars < min {
        return Err(PolicyViolation::TooShort(min));
    }
    if password.len() > MAX_LENGTH {
        return Err(PolicyViolation::TooLong);
    }
    let lower = password.to_lowercase();
    let user = username.trim().to_lowercase();
    if user.chars().count() >= 3 && lower.contains(&user) {
        return Err(PolicyViolation::ContainsUsername);
    }
    let mut it = password.chars();
    if let Some(first) = it.next()
        && it.all(|c| c == first)
    {
        return Err(PolicyViolation::Repetitive);
    }
    if is_common(&lower) {
        return Err(PolicyViolation::Common);
    }
    Ok(())
}

/// Whether the (lower-cased) password is on the shipped list.
fn is_common(lower: &str) -> bool {
    BREACHED
        .lines()
        .filter(|l| !l.starts_with('#'))
        .any(|l| l == lower)
}

/// A random password for "reset password" / "create user": 20 characters
/// from an unambiguous alphabet (~114 bits). Shown once to the admin.
pub fn generate_password() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::rngs::OsRng;
    (0..20)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_argon2id_with_owasp_parameters_and_verifies() {
        let h = hash_password("correct horse battery staple");
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{h}");
        assert!(verify_password("correct horse battery staple", &h));
        assert!(!verify_password("correct horse battery stapl", &h));
        assert!(!verify_password("x", "not a phc string"));
        // Salted: two hashes of one password differ.
        assert_ne!(h, hash_password("correct horse battery staple"));
        assert!(!verify_password(&"a".repeat(MAX_LENGTH + 1), &h));
        assert!(!verify_password("anything", dummy_hash()));
    }

    #[test]
    fn policy() {
        assert_eq!(
            check_policy("short", "bob", 12),
            Err(PolicyViolation::TooShort(12))
        );
        // Floor of 8 even when configured lower.
        assert_eq!(
            check_policy("abcdefg", "bob", 4),
            Err(PolicyViolation::TooShort(8))
        );
        assert_eq!(
            check_policy("xxAliceSmith-2026", "alicesmith", 12),
            Err(PolicyViolation::ContainsUsername)
        );
        assert_eq!(
            check_policy("aaaaaaaaaaaaaaa", "bob", 12),
            Err(PolicyViolation::Repetitive)
        );
        assert_eq!(
            check_policy("Unbelievable", "bob", 8),
            Err(PolicyViolation::Common)
        );
        assert_eq!(
            check_policy(&"ab".repeat(200), "bob", 12),
            Err(PolicyViolation::TooLong)
        );
        assert!(check_policy("tidy-Lantern-41-orbit", "bob", 12).is_ok());
        for v in [
            PolicyViolation::TooShort(1),
            PolicyViolation::TooLong,
            PolicyViolation::ContainsUsername,
            PolicyViolation::Repetitive,
            PolicyViolation::Common,
        ] {
            assert!(v.message_id().starts_with("console-password-"));
            assert!(!v.to_string().is_empty());
        }
    }

    #[test]
    fn list_is_loaded() {
        assert!(BREACHED.lines().filter(|l| !l.starts_with('#')).count() > 2000);
        assert!(is_common("password1"));
        assert!(!is_common("tidy-lantern-41-orbit"));
    }

    #[test]
    fn generated_passwords_pass_the_policy() {
        for _ in 0..20 {
            let p = generate_password();
            assert_eq!(p.len(), 20);
            assert!(check_policy(&p, "admin", 12).is_ok(), "{p}");
        }
    }
}
