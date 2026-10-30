//! Codes, tokens and names: how they are made, typed, compared and stored.
//!
//! Secrets are kept as hashes only (floor 4): a token, an install code and a
//! request's handle as their SHA-256 (all random, 80 bits or more), the wake
//! code as an Argon2id PHC string (an owner may choose a weak one).
//! Plaintext leaves the engine once, in the response that created it.

use anyhow::{bail, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Crockford base32: no I, L, O or U, so a code read aloud survives.
pub const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// The install code (E-02): 16 symbols, 80 bits. It pairs the primary and is
/// made only over SSH. Edge allows 5 tries a minute per address; it has no
/// try limit of its own, since burning it would let anyone cost an owner
/// without SSH their only way in.
pub const INSTALL_CODE_LEN: usize = 16;
/// The code a new Manager shows and the owner types into the primary: 8
/// symbols. Never guessed against: the owner types it, and a request no one
/// approves just expires.
pub const REQUEST_CODE_LEN: usize = 8;
/// A generated wake code: 16 symbols, 80 bits.
pub const WAKE_CODE_LEN: usize = 16;
/// The shortest wake code an owner may choose.
pub const WAKE_MIN_LEN: usize = 12;
/// The longest code field the API reads; anything longer is refused before
/// it is hashed.
pub const CODE_FIELD_MAX: usize = 128;
pub const TOKEN_PREFIX: &str = "aet_";
/// A request handle: 32 random bytes, hex.
pub const HANDLE_LEN: usize = 64;
pub const NAME_MAX: usize = 64;
pub const PAIRING_ID_LEN: usize = 12;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("the OS random source is unavailable");
    b
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// Compare without an early exit, so the time taken says nothing about where
/// two secrets differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `n` Crockford symbols from the OS random source. 256 is a multiple of 32,
/// so taking the low five bits has no bias.
pub fn mint_symbols(n: usize) -> String {
    (0..n)
        .map(|_| CODE_ALPHABET[(random_bytes::<1>()[0] & 31) as usize] as char)
        .collect()
}

/// What a person typed, made comparable: case, spaces and dashes don't matter,
/// and the look-alikes Crockford drops read as the digits they resemble.
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            u => u,
        })
        .collect()
}

/// `ABCD-EFGH`: how a code is shown.
pub fn group(code: &str, size: usize) -> String {
    code.as_bytes()
        .chunks(size)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn new_token() -> String {
    format!("{TOKEN_PREFIX}{}", hex(&random_bytes::<32>()))
}

/// Whether `t` has a token's shape (what edge's `aet_[0-9a-f]{64}` map checks).
pub fn is_token_shaped(t: &str) -> bool {
    t.strip_prefix(TOKEN_PREFIX)
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn new_handle() -> String {
    hex(&random_bytes::<32>())
}

pub fn is_handle_shaped(h: &str) -> bool {
    h.len() == HANDLE_LEN && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn new_pairing_id() -> String {
    mint_symbols(PAIRING_ID_LEN).to_ascii_lowercase()
}

/// A pairing id as the API accepts it: exactly what [`new_pairing_id`] makes.
pub fn is_pairing_id(s: &str) -> bool {
    s.len() == PAIRING_ID_LEN
        && s.bytes()
            .all(|b| !b.is_ascii_uppercase() && CODE_ALPHABET.contains(&b.to_ascii_uppercase()))
}

/// A pairing name as people will read it in a list: 1 to 64 characters, no
/// control or invisible format characters (bidi overrides, zero-width), so
/// one pairing can't pose as another.
pub fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=NAME_MAX).contains(&n)
        && !name.chars().any(|c| {
            c.is_control()
                || matches!(c as u32,
                    0x00AD | 0x061C | 0x180E | 0x200B..=0x200F | 0x2028..=0x202E
                    | 0x2060..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB)
        })
}

/// An install code, shown as four groups of four.
pub fn mint_install_code() -> String {
    group(&mint_symbols(INSTALL_CODE_LEN), 4)
}

/// A generated wake code: 16 Crockford symbols, four groups of four. (The
/// design says four diceware words; that needs a word list, not shipped yet.)
pub fn mint_wake_code() -> String {
    group(&mint_symbols(WAKE_CODE_LEN), 4)
}

/// How a wake code is compared. A generated one forgives case, dashes and
/// look-alikes like every other code; an owner's own is taken as typed apart
/// from surrounding whitespace. Which one is stored with the hash, so an
/// owner's code that happens to look generated isn't case-folded.
pub fn normalize_wake(code: &str, generated: bool) -> String {
    if generated {
        normalize_code(code)
    } else {
        code.trim().to_string()
    }
}

/// Hash a wake code for storage. Refuses one shorter than [`WAKE_MIN_LEN`]
/// or longer than [`CODE_FIELD_MAX`].
pub fn hash_wake_code(code: &str, generated: bool) -> Result<String> {
    let code = normalize_wake(code, generated);
    let n = code.chars().count();
    if n < WAKE_MIN_LEN {
        bail!("a wake code needs at least {WAKE_MIN_LEN} characters");
    }
    if code.len() > CODE_FIELD_MAX {
        bail!("a wake code has at most {CODE_FIELD_MAX} bytes");
    }
    let salt = SaltString::encode_b64(&random_bytes::<16>()).map_err(|e| anyhow::anyhow!(e))?;
    Ok(Argon2::default()
        .hash_password(code.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!(e))?
        .to_string())
}

/// Verify a typed wake code. With no stored code, `dummy` is verified instead,
/// so "no code is set" takes as long as "wrong code".
pub fn verify_wake_code(stored: Option<&str>, generated: bool, typed: &str, dummy: &str) -> bool {
    let typed = normalize_wake(typed, generated);
    let ok = PasswordHash::new(stored.unwrap_or(dummy))
        .map(|h| {
            Argon2::default()
                .verify_password(typed.as_bytes(), &h)
                .is_ok()
        })
        .unwrap_or(false);
    ok && stored.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_codes_have_eighty_bits_from_the_whole_alphabet() {
        let c = mint_install_code();
        assert_eq!(c.len(), INSTALL_CODE_LEN + 3, "four groups of four");
        assert!(normalize_code(&c)
            .bytes()
            .all(|b| CODE_ALPHABET.contains(&b)));
        assert_eq!(CODE_ALPHABET.len(), 32, "5 bits a symbol");
    }

    #[test]
    fn typed_codes_forgive_case_dashes_and_look_alikes() {
        assert_eq!(normalize_code(" ab1o0-il xy "), "AB10011XY");
    }

    #[test]
    fn ids_tokens_and_handles_are_shape_checked() {
        assert!(is_pairing_id(&new_pairing_id()));
        assert!(!is_pairing_id("../../etc/pw"));
        assert!(!is_pairing_id("ABCDEFGHJKMN"), "upper case is not an id");
        assert!(!is_pairing_id("abcdefghjkmnp"), "13 long");
        assert!(!is_pairing_id("abcdefghijkl"), "i and l aren't in it");
        assert!(is_token_shaped(&new_token()));
        assert!(!is_token_shaped("aet_short"));
        assert!(!is_token_shaped(&format!("aet_{}", "g".repeat(64))));
        assert!(is_handle_shaped(&new_handle()));
        assert!(!is_handle_shaped(&"A".repeat(64)));
    }

    #[test]
    fn names_refuse_invisible_and_bidi_characters() {
        assert!(valid_name("Shibin's MacBook"));
        assert!(valid_name(&"é".repeat(64)), "64 characters, not bytes");
        assert!(!valid_name("Shibin\u{202E}s MacBook"));
        assert!(!valid_name("a\u{200B}b"));
        assert!(!valid_name("a\nb"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[test]
    fn wake_codes_verify_and_unset_never_matches() {
        let dummy = hash_wake_code(&mint_wake_code(), true).unwrap();
        let code = mint_wake_code();
        let stored = hash_wake_code(&code, true).unwrap();
        assert!(!stored.contains(&normalize_code(&code)));
        assert!(verify_wake_code(
            Some(&stored),
            true,
            &code.to_lowercase(),
            &dummy
        ));
        assert!(!verify_wake_code(
            Some(&stored),
            true,
            "nope nope nope",
            &dummy
        ));
        assert!(!verify_wake_code(None, true, &code, &dummy));
        assert!(hash_wake_code("short", false).is_err());
        assert!(hash_wake_code(&"x".repeat(CODE_FIELD_MAX + 1), false).is_err());
    }

    #[test]
    fn an_owners_own_wake_code_is_case_sensitive_even_if_it_looks_generated() {
        // The POC folded case for any 19-character code with three dashes.
        let own = "Abcd-Efgh-Jkmn-Pqrs";
        let dummy = hash_wake_code(&mint_wake_code(), true).unwrap();
        let stored = hash_wake_code(own, false).unwrap();
        assert!(verify_wake_code(Some(&stored), false, own, &dummy));
        assert!(!verify_wake_code(
            Some(&stored),
            false,
            &own.to_uppercase(),
            &dummy
        ));
        assert!(verify_wake_code(
            Some(&stored),
            false,
            &format!("  {own}\n"),
            &dummy
        ));
    }
}
