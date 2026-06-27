//! Location and on-disk layout of an Atelier appliance stack.
//!
//! The stack is a directory (default `~/.atelier`, override with `ATELIER_HOME`;
//! a legacy `~/.aincient` is still honoured if present) holding a `compose.yaml`
//! + `.env` — the same pair `docker/install.sh` writes.
//! The manager owns this directory so the CLI and GUI converge on one source of truth.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default image tag, matching `docker/install.sh`.
pub const DEFAULT_IMAGE: &str = "ghcr.io/aincient-labs/cms:edge";
/// Default console port — "AINCI" in leet (4=A,1=I,2=N,2=C,1=I).
pub const DEFAULT_PORT: u16 = 41221;

/// The Compose stack written into the stack directory. Kept byte-for-byte in
/// step with the `cat > compose.yaml` heredoc in `docker/install.sh`: the slim
/// runtime topology (app + db, no build context, no updater sidecar).
pub const COMPOSE_TEMPLATE: &str = r#"name: aincient
services:
  db:
    image: mariadb:11
    environment:
      MARIADB_DATABASE: aincient
      MARIADB_USER: aincient
      MARIADB_PASSWORD: ${DB_PASSWORD:-aincient}
      MARIADB_RANDOM_ROOT_PASSWORD: "yes"
    volumes:
      - db-data:/var/lib/mysql
    healthcheck:
      test: ["CMD", "healthcheck.sh", "--connect"]
      interval: 10s
      retries: 10
  app:
    image: ${AINCIENT_IMAGE:-ghcr.io/aincient-labs/cms:edge}
    depends_on:
      db:
        condition: service_healthy
    environment:
      DATABASE_URL: mysql://aincient:${DB_PASSWORD:-aincient}@db/aincient
      HASH_SALT: ${HASH_SALT:?set HASH_SALT in .env}
      AINCIENT_AI_KEY: ${AINCIENT_AI_KEY:-}
      AINCIENT_TRUSTED_HOSTS: ${AINCIENT_TRUSTED_HOSTS:-}
      AINCIENT_ADMIN_PASS: ${ADMIN_PASS:-}
    ports:
      - "${HTTP_PORT:-41221}:80"
    volumes:
      - files:/opt/drupal/web/sites/default/files
      - private:/opt/drupal/private
    restart: unless-stopped
volumes:
  db-data:
  files:
  private:
"#;

/// A located appliance stack directory.
#[derive(Debug, Clone)]
pub struct Stack {
    pub home: PathBuf,
}

/// Options for scaffolding a fresh stack.
#[derive(Debug, Default, Clone)]
pub struct InstallOptions {
    /// Provider-neutral AI API key (the `AINCIENT_AI_KEY` bootstrap seed).
    /// `None` leaves it blank — the in-app onboarding wizard then prompts for a
    /// provider + credential on first run.
    pub ai_key: Option<String>,
    /// Override image tag (defaults to [`DEFAULT_IMAGE`]).
    pub image: Option<String>,
    /// Override console port (defaults to [`DEFAULT_PORT`]).
    pub http_port: Option<u16>,
}

impl Stack {
    /// Resolve the stack directory.
    ///
    /// Precedence: `ATELIER_HOME`, then the legacy `AINCIENT_HOME` (so existing
    /// environments keep working). With neither set, default to `~/.atelier` —
    /// but if that doesn't yet exist and a legacy `~/.aincient` does, adopt the
    /// legacy directory so beta installs aren't orphaned.
    pub fn locate() -> Result<Self> {
        let env_override = std::env::var_os("ATELIER_HOME")
            .or_else(|| std::env::var_os("AINCIENT_HOME"));
        let home = match env_override {
            Some(p) => PathBuf::from(p),
            None => {
                let base = dirs::home_dir()
                    .context("could not determine your home directory")?;
                let new = base.join(".atelier");
                let legacy = base.join(".aincient");
                if !new.exists() && legacy.exists() {
                    legacy
                } else {
                    new
                }
            }
        };
        Ok(Self { home })
    }

    pub fn compose_path(&self) -> PathBuf {
        self.home.join("compose.yaml")
    }

    pub fn env_path(&self) -> PathBuf {
        self.home.join(".env")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.home.join("backups")
    }

    /// True once a `compose.yaml` exists — i.e. the stack has been laid down.
    pub fn exists(&self) -> bool {
        self.compose_path().is_file()
    }

    /// Parse the `.env` into key/value pairs. Missing file → empty map.
    pub fn read_env(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        let Ok(text) = std::fs::read_to_string(self.env_path()) else {
            return map;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        map
    }

    pub fn env_get(&self, key: &str) -> Option<String> {
        self.read_env().get(key).cloned().filter(|v| !v.is_empty())
    }

    pub fn http_port(&self) -> u16 {
        self.env_get("HTTP_PORT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PORT)
    }

    /// URL of the AIncient **console** (the chat workspace at `/aincient`), not the
    /// bare site root — this is what "open console" should land on.
    pub fn console_url(&self) -> String {
        format!("http://localhost:{}/aincient", self.http_port())
    }

    /// URL of Drupal's login form. "Login" sends the operator straight here to
    /// sign in themselves — the manager never displays the admin password.
    pub fn login_url(&self) -> String {
        format!("http://localhost:{}/user/login", self.http_port())
    }

    pub fn image(&self) -> String {
        self.env_get("AINCIENT_IMAGE")
            .unwrap_or_else(|| DEFAULT_IMAGE.to_string())
    }

    /// Lay down `compose.yaml` + `.env` if absent. Never clobbers an existing
    /// `.env` (preserves `HASH_SALT`/`AINCIENT_AI_KEY`); reconciles the image
    /// and port tunables on a re-run, mirroring `install.sh`.
    pub fn ensure_scaffold(&self, opts: &InstallOptions) -> Result<()> {
        std::fs::create_dir_all(&self.home)
            .with_context(|| format!("could not create stack directory {}", self.home.display()))?;

        let compose = self.compose_path();
        if !compose.is_file() {
            std::fs::write(&compose, COMPOSE_TEMPLATE)
                .with_context(|| format!("could not write {}", compose.display()))?;
        }

        let image = opts.image.clone().unwrap_or_else(|| DEFAULT_IMAGE.to_string());
        let port = opts.http_port.unwrap_or(DEFAULT_PORT);
        let env_path = self.env_path();

        if !env_path.is_file() {
            let key = opts.ai_key.clone().unwrap_or_default();
            let contents = format!(
                "HASH_SALT={salt}\n\
                 AINCIENT_AI_KEY={key}\n\
                 AINCIENT_IMAGE={image}\n\
                 HTTP_PORT={port}\n\
                 ADMIN_PASS=\n",
                salt = hash_salt(),
            );
            write_private(&env_path, &contents)?;
        } else {
            // Reconcile tunables this run may have changed; keep secrets on disk.
            let mut env = self.read_env();
            env.insert("AINCIENT_IMAGE".to_string(), image.clone());
            env.insert("HTTP_PORT".to_string(), port.to_string());
            if let Some(key) = &opts.ai_key {
                if !key.is_empty() {
                    env.insert("AINCIENT_AI_KEY".to_string(), key.clone());
                }
            }
            let body: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
            write_private(&env_path, &body)?;
        }
        Ok(())
    }
}

/// Write a file containing secrets with `0600` perms where the platform supports it.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)
        .with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 32 random bytes, hex-encoded — a Drupal `HASH_SALT` (matches `openssl rand -hex 32`).
fn hash_salt() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("OS RNG unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temp directory per test, cleaned up on drop.
    struct TempStack(Stack);
    impl TempStack {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "aincient-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            TempStack(Stack { home: dir })
        }
    }
    impl Drop for TempStack {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.home);
        }
    }

    #[test]
    fn scaffold_writes_compose_and_env() {
        let ts = TempStack::new();
        let stack = &ts.0;
        assert!(!stack.exists());

        stack.ensure_scaffold(&InstallOptions::default()).unwrap();

        assert!(stack.exists());
        assert_eq!(std::fs::read_to_string(stack.compose_path()).unwrap(), COMPOSE_TEMPLATE);

        let env = stack.read_env();
        assert_eq!(env.get("HASH_SALT").unwrap().len(), 64);
        assert!(env.get("HASH_SALT").unwrap().chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(env.get("AINCIENT_IMAGE").map(String::as_str), Some(DEFAULT_IMAGE));
        assert_eq!(env.get("HTTP_PORT").map(String::as_str), Some("41221"));
        assert_eq!(env.get("AINCIENT_AI_KEY").map(String::as_str), Some(""));
    }

    #[test]
    fn scaffold_records_provided_key_and_port() {
        let ts = TempStack::new();
        let opts = InstallOptions {
            ai_key: Some("sk-test".into()),
            image: None,
            http_port: Some(8080),
        };
        ts.0.ensure_scaffold(&opts).unwrap();

        assert_eq!(ts.0.env_get("AINCIENT_AI_KEY").as_deref(), Some("sk-test"));
        assert_eq!(ts.0.http_port(), 8080);
        assert_eq!(ts.0.console_url(), "http://localhost:8080/aincient");
    }

    #[test]
    fn re_scaffold_preserves_secrets_but_reconciles_tunables() {
        let ts = TempStack::new();
        let stack = &ts.0;
        ts.0.ensure_scaffold(&InstallOptions {
            ai_key: Some("sk-secret".into()),
            ..Default::default()
        })
        .unwrap();
        let salt = stack.env_get("HASH_SALT").unwrap();

        // Re-run pointing at a new image + port, supplying no key.
        stack
            .ensure_scaffold(&InstallOptions {
                ai_key: None,
                image: Some("ghcr.io/aincient-labs/cms:v2".into()),
                http_port: Some(9000),
            })
            .unwrap();

        assert_eq!(stack.env_get("HASH_SALT"), Some(salt), "salt must be preserved");
        assert_eq!(stack.env_get("AINCIENT_AI_KEY").as_deref(), Some("sk-secret"));
        assert_eq!(stack.image(), "ghcr.io/aincient-labs/cms:v2");
        assert_eq!(stack.http_port(), 9000);
    }

    #[test]
    fn defaults_apply_without_a_stack() {
        let ts = TempStack::new();
        assert_eq!(ts.0.image(), DEFAULT_IMAGE);
        assert_eq!(ts.0.http_port(), DEFAULT_PORT);
        assert!(ts.0.read_env().is_empty());
    }
}
