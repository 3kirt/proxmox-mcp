//! Classification against the *real* `gpg`, not the fake one: catches drift
//! in GnuPG's status codes, which every other test only replays from a
//! capture. Opt-in (`make test-real-gpg`, i.e. `REAL_GPG=1 cargo test --
//! --ignored real_gpg`) because it generates keys and starts a gpg-agent.
//!
//! The GnuPG home is a fresh temp dir with a short path: gpg-agent's socket
//! lives inside it, and macOS limits socket paths to 104 bytes.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{PassError, PassStore};

const TOKEN: &str = "realgpg@pve!test=not-a-real-secret";

fn gpg_bin() -> PathBuf {
    std::env::var_os("GPG").map_or_else(|| PathBuf::from("gpg"), PathBuf::from)
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("run gpg");
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn gpg(home: &Path) -> Command {
    let mut cmd = Command::new(gpg_bin());
    cmd.arg("--homedir").arg(home).args(["--batch", "--no-tty"]);
    cmd
}

/// `uid` with a cv25519 encryption subkey, protected by `passphrase` ("" = none).
fn gen_key(home: &Path, uid: &str, passphrase: &str) {
    run(gpg(home).args([
        "--pinentry-mode",
        "loopback",
        "--passphrase",
        passphrase,
        "--quick-gen-key",
        uid,
        "default",
        "default",
        "never",
    ]));
}

/// A short-path GnuPG home; its agent is killed on drop.
struct Home(tempfile::TempDir);

impl Home {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("pmxg")
            .tempdir()
            .expect("temp GnuPG home");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn gpgconf(&self, arg: &str) {
        let gpgconf = gpg_bin()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("gpgconf"), |dir| dir.join("gpgconf"));
        let _ = Command::new(gpgconf)
            .arg("--homedir")
            .arg(self.path())
            .arg(arg)
            .arg("gpg-agent")
            .status();
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.gpgconf("--kill");
    }
}

#[tokio::test]
#[ignore = "needs a real gpg; run with `make test-real-gpg`"]
async fn real_gpg_status_codes_classify_as_expected() {
    if std::env::var_os("REAL_GPG").is_none() {
        eprintln!("REAL_GPG not set; skipping");
        return;
    }
    let home = Home::new();
    gen_key(home.path(), "Open <open@example.invalid>", "");
    gen_key(
        home.path(),
        "Locked <locked@example.invalid>",
        "real-gpg-test-pp",
    );

    // A key whose secret half lives only in another keyring.
    let foreign = Home::new();
    gen_key(foreign.path(), "Foreign <foreign@example.invalid>", "");
    let public = gpg(foreign.path())
        .args(["--armor", "--export", "foreign@example.invalid"])
        .output()
        .unwrap()
        .stdout;
    let mut import = gpg(home.path())
        .arg("--import")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(import.stdin.as_mut().unwrap(), &public).unwrap();
    assert!(
        import.wait().unwrap().success(),
        "importing the foreign key"
    );

    let store_dir = tempfile::tempdir().unwrap();
    let store = PassStore::new(store_dir.path(), gpg_bin()).with_homedir(Some(home.path().into()));

    let open = store.entry("open").unwrap();
    open.write_token(TOKEN, &["open@example.invalid".into()])
        .await
        .unwrap();
    let locked = store.entry("locked").unwrap();
    locked
        .write_token(TOKEN, &["locked@example.invalid".into()])
        .await
        .unwrap();
    // The imported key isn't trusted, so encrypt to it directly.
    let foreign_entry = store.entry("foreign").unwrap();
    let mut enc = gpg(home.path())
        .args(["--trust-model", "always", "--encrypt", "--recipient"])
        .arg("foreign@example.invalid")
        .arg("--output")
        .arg(foreign_entry.path())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(enc.stdin.as_mut().unwrap(), format!("{TOKEN}\n").as_bytes())
        .unwrap();
    assert!(
        enc.wait().unwrap().success(),
        "encrypting to the foreign key"
    );

    // Generating with a loopback passphrase may have cached it: forget it.
    home.gpgconf("--reload");

    assert_eq!(
        open.read_token().await.unwrap(),
        TOKEN,
        "DECRYPTION_OKAY path"
    );
    let err = locked.read_token().await.unwrap_err();
    assert!(
        matches!(err, PassError::Locked { .. }),
        "expected Locked, got {err}"
    );
    let err = foreign_entry.read_token().await.unwrap_err();
    assert!(
        matches!(err, PassError::NoSecretKey { .. }),
        "expected NoSecretKey, got {err}"
    );
}
