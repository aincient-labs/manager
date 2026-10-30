//! Guided per-server SSH keys (M2, D-05).
//!
//! The alternative to an existing `~/.ssh/config` alias: the Manager generates
//! an ed25519 key for one server, shows its public line and the exact command
//! that authorises it there, and keeps the **private key in the OS keychain**
//! (macOS Keychain, Linux Secret Service, Windows Credential Manager) under
//! the ssh target. It is never stored on disk: for an ssh call it is written to
//! a 0600 file in a fresh 0700 temp directory ([`KeyFile`]) that is removed
//! when the call's transport is dropped. Without a keychain this fails with a
//! message pointing at the alias route instead.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::docker::output_within;
use crate::remote::PrivateDir;

/// The keychain service every guided key is filed under; the account is the
/// ssh target.
pub const KEYCHAIN_SERVICE: &str = "atelier-manager-ssh";

/// Where guided private keys are kept.
pub trait SecretStore {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

/// The OS keychain, via the `keyring` crate.
pub struct Keychain;

fn no_keychain(host: &str, why: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(
        "no OS keychain is available to hold the key for {host} ({why}). The Manager never \
         stores keys on disk — use an SSH alias instead: add the server to ~/.ssh/config, check \
         `ssh <alias>` works, then `atelier sites add <SLUG> --host <alias>`"
    )
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
impl SecretStore for Keychain {
    fn get(&self, account: &str) -> Result<Option<String>> {
        let entry =
            keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(|e| no_keychain(account, e))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(no_keychain(account, e)),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        keyring::Entry::new(KEYCHAIN_SERVICE, account)
            .and_then(|entry| entry.set_password(secret))
            .map_err(|e| no_keychain(account, e))
    }

    fn delete(&self, account: &str) -> Result<()> {
        let entry =
            keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(|e| no_keychain(account, e))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(no_keychain(account, e)),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
impl SecretStore for Keychain {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Err(no_keychain(account, "unsupported platform"))
    }
    fn set(&self, account: &str, _: &str) -> Result<()> {
        Err(no_keychain(account, "unsupported platform"))
    }
    fn delete(&self, account: &str) -> Result<()> {
        Err(no_keychain(account, "unsupported platform"))
    }
}

/// A private key materialised for one ssh call: `<0700 temp dir>/id_ed25519`
/// at 0600. The directory and the key go when this is dropped.
pub struct KeyFile {
    path: PathBuf,
    _dir: PrivateDir,
}

impl KeyFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Write `private` to a fresh [`KeyFile`].
pub fn materialise(private: &str) -> Result<KeyFile> {
    let dir = PrivateDir::new("atelier-ssh")?;
    let path = dir.path().join("id_ed25519");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("could not create {}", path.display()))?;
    std::io::Write::write_all(&mut file, private.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(KeyFile { path, _dir: dir })
}

/// The guided key for `host`, materialised — or an error saying to set it up.
pub fn key_file(store: &dyn SecretStore, host: &str) -> Result<KeyFile> {
    match store.get(host)? {
        Some(private) => materialise(&private),
        None => bail!(
            "the Manager has no key for {host} in the keychain — run \
             `atelier sites add <SLUG> --host {host} --key guided` to set one up"
        ),
    }
}

/// A guided key, ready to be authorised on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidedKey {
    /// The `ssh-ed25519 AAAA… comment` line for `authorized_keys`.
    pub public_line: String,
    /// It was generated just now (rather than found in the keychain).
    pub created: bool,
}

/// The guided key for `host`: the keychain's, or a new ed25519 key generated
/// with `ssh-keygen` in a private temp dir and stored in the keychain (the
/// files are removed straight after). Reusing an existing key means a retry
/// after an interrupted setup doesn't need a second `authorized_keys` line.
pub fn ensure_guided_key(store: &dyn SecretStore, host: &str) -> Result<GuidedKey> {
    if let Some(private) = store.get(host)? {
        return Ok(GuidedKey {
            public_line: public_line_of(&private)?,
            created: false,
        });
    }
    let dir = PrivateDir::new("atelier-keygen")?;
    let path = dir.path().join("id_ed25519");
    let mut cmd = Command::new("ssh-keygen");
    cmd.args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-C")
        .arg(format!("atelier-manager@{host}"))
        .arg("-f")
        .arg(&path);
    run_keygen(&mut cmd, "generating a key")?;
    let private = std::fs::read_to_string(&path).context("could not read the new key")?;
    let public = std::fs::read_to_string(path.with_extension("pub"))
        .context("could not read the new public key")?;
    store.set(host, &private)?;
    Ok(GuidedKey {
        public_line: public.trim().to_string(),
        created: true,
    })
}

/// Forget `host`'s guided key.
pub fn forget_key(store: &dyn SecretStore, host: &str) -> Result<()> {
    store.delete(host)
}

/// The public line for a private key (`ssh-keygen -y`).
fn public_line_of(private: &str) -> Result<String> {
    let key = materialise(private)?;
    let mut cmd = Command::new("ssh-keygen");
    cmd.arg("-y").arg("-f").arg(key.path());
    let out = run_keygen(&mut cmd, "reading the stored key")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_keygen(cmd: &mut Command, what: &str) -> Result<std::process::Output> {
    let out = output_within(cmd, Duration::from_secs(30))
        .map_err(|e| anyhow!("{what}: {e} — is OpenSSH (ssh-keygen) installed?"))?;
    if !out.status.success() {
        bail!(
            "{what}: ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

/// The command to run **on the server** that authorises `public_line`.
pub fn authorize_command(public_line: &str) -> String {
    format!(
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && echo {} >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys",
        crate::remote::sh_quote(public_line.trim())
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    pub(crate) struct Memory(RefCell<HashMap<String, String>>);
    impl SecretStore for Memory {
        fn get(&self, account: &str) -> Result<Option<String>> {
            Ok(self.0.borrow().get(account).cloned())
        }
        fn set(&self, account: &str, secret: &str) -> Result<()> {
            self.0
                .borrow_mut()
                .insert(account.to_string(), secret.to_string());
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<()> {
            self.0.borrow_mut().remove(account);
            Ok(())
        }
    }

    struct Broken;
    impl SecretStore for Broken {
        fn get(&self, account: &str) -> Result<Option<String>> {
            Err(no_keychain(
                account,
                "no Secret Service on this session bus",
            ))
        }
        fn set(&self, account: &str, _: &str) -> Result<()> {
            Err(no_keychain(
                account,
                "no Secret Service on this session bus",
            ))
        }
        fn delete(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn have_keygen() -> bool {
        Command::new("ssh-keygen").arg("-?").output().is_ok()
    }

    #[test]
    fn a_guided_key_is_generated_once_and_kept_only_in_the_store() {
        if !have_keygen() {
            eprintln!("skipping: no ssh-keygen");
            return;
        }
        let store = Memory::default();
        let first = ensure_guided_key(&store, "deploy@example.com").unwrap();
        assert!(first.created);
        assert!(
            first.public_line.starts_with("ssh-ed25519 ")
                && first
                    .public_line
                    .ends_with(" atelier-manager@deploy@example.com"),
            "{}",
            first.public_line
        );
        let private = store.get("deploy@example.com").unwrap().unwrap();
        assert!(private.contains("OPENSSH PRIVATE KEY"));
        let again = ensure_guided_key(&store, "deploy@example.com").unwrap();
        assert!(!again.created);
        assert_eq!(again.public_line, first.public_line);
        forget_key(&store, "deploy@example.com").unwrap();
        assert_eq!(store.get("deploy@example.com").unwrap(), None);
    }

    #[test]
    fn no_keychain_means_a_clear_error_and_nothing_on_disk() {
        let err = format!("{:#}", ensure_guided_key(&Broken, "h").unwrap_err());
        assert!(
            err.contains("no OS keychain") && err.contains("SSH alias"),
            "{err}"
        );
        let err = format!("{:#}", key_file(&Memory::default(), "h").err().unwrap());
        assert!(err.contains("--key guided"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_materialised_key_is_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let key = materialise("secret\n").unwrap();
        let path = key.path().to_path_buf();
        let dir = path.parent().unwrap().to_path_buf();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret\n");
        drop(key);
        assert!(!dir.exists());
    }

    #[test]
    fn the_authorize_command_appends_the_quoted_line() {
        assert_eq!(
            authorize_command("ssh-ed25519 AAAA atelier-manager@h\n"),
            "mkdir -p ~/.ssh && chmod 700 ~/.ssh && echo 'ssh-ed25519 AAAA atelier-manager@h' \
             >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys"
        );
    }
}
