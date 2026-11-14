//! A minimal client for gpg-agent's Assuan protocol, used to report whether
//! the token's decryption key is unlocked *without* decrypting anything.
//!
//! This is deliberately diagnostic only (`proxmox-mcp status`): the server's
//! token reads go straight to a `--pinentry-mode error` decrypt, because
//! "not cached" is not the same as "can't decrypt" — a passphrase-less key or
//! a smartcard key reports uncached yet decrypts fine.
//!
//! Assuan is line-based: the agent greets with `OK …`, and each command is
//! answered by any number of `S` (status), `D` (data) and `#` (comment) lines,
//! terminated by `OK` or `ERR <code> <description>`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::PassStore;

const AGENT_TIMEOUT: Duration = Duration::from_secs(5);

/// gpg-error code for "Not found" (the agent holds no such key).
const GPG_ERR_NOT_FOUND: u32 = 27;

/// The `KEYINFO` fields this crate cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// `D` = key stored on disk, `T` = smartcard, `X` = unknown.
    pub kind: char,
    /// Passphrase currently in gpg-agent's cache.
    pub cached: bool,
    /// `P` = passphrase-protected, `C` = not protected, `-` = unknown.
    pub protection: char,
}

/// Summary of whether the token's key can decrypt without a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Unlocked,
    Unprotected,
    Locked,
    Smartcard,
    NotRunning,
    NoSecretKey,
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unlocked => "unlocked (passphrase cached in gpg-agent)",
            Self::Unprotected => "unlocked (key has no passphrase)",
            Self::Locked => "locked (run `proxmox-mcp unlock`)",
            Self::Smartcard => "smartcard key (gpg-agent does not report PIN state)",
            Self::NotRunning => "locked (gpg-agent is not running; run `proxmox-mcp unlock`)",
            Self::NoSecretKey => "no secret key for the store's recipients in this keyring",
        })
    }
}

/// Reduce per-keygrip results (`None` = agent doesn't hold that key) to one
/// state. Any usable key wins, since gpg tries every recipient.
pub fn summarize(infos: &[Option<KeyInfo>]) -> AgentState {
    let held: Vec<&KeyInfo> = infos.iter().flatten().collect();
    if held.iter().any(|k| k.cached) {
        AgentState::Unlocked
    } else if held.iter().any(|k| k.protection == 'C') {
        AgentState::Unprotected
    } else if held.iter().any(|k| k.kind == 'T') {
        AgentState::Smartcard
    } else if held.is_empty() {
        AgentState::NoSecretKey
    } else {
        AgentState::Locked
    }
}

/// Keygrips of the encryption-capable (sub)keys for `recipients`, from
/// `gpg --list-secret-keys --with-colons --with-keygrip`.
pub async fn encryption_keygrips(
    store: &PassStore,
    recipients: &[String],
) -> Result<Vec<String>, String> {
    let output = tokio::time::timeout(
        AGENT_TIMEOUT,
        store
            .gpg_command()
            .args([
                "--batch",
                "--no-tty",
                "--with-colons",
                "--with-keygrip",
                "--list-secret-keys",
                "--",
            ])
            .args(recipients)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "timed out listing secret keys".to_string())?
    .map_err(|e| format!("running gpg: {e}"))?;
    // A recipient with no secret key makes gpg exit non-zero while still
    // listing the others, so parse whatever came out rather than failing.
    Ok(parse_encryption_keygrips(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// In colon listings a `grp` record carries the keygrip (field 10) of the
/// `sec`/`ssb` record before it, whose field 12 lists its capabilities;
/// a lowercase `e` means that specific (sub)key can encrypt. Field 2 is its
/// validity: an expired (`e`), revoked (`r`), disabled (`d`) or invalid (`i`)
/// subkey is skipped, since gpg won't decrypt with it, so its cached
/// passphrase must not make `status` report "unlocked".
fn parse_encryption_keygrips(colons: &str) -> Vec<String> {
    let mut grips = Vec::new();
    let mut want = false;
    for line in colons.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.first().copied() {
            Some("sec" | "ssb") => {
                let usable = !fields
                    .get(1)
                    .is_some_and(|v| matches!(*v, "e" | "r" | "d" | "i"));
                want = usable && fields.get(11).is_some_and(|caps| caps.contains('e'));
            }
            Some("grp") => {
                if want && let Some(grip) = fields.get(9).filter(|g| !g.is_empty()) {
                    grips.push((*grip).to_string());
                }
                want = false;
            }
            _ => {}
        }
    }
    grips
}

/// The agent's socket path, from `gpgconf --list-dirs agent-socket`.
pub async fn agent_socket(store: &PassStore) -> Result<PathBuf, String> {
    let output = tokio::time::timeout(
        AGENT_TIMEOUT,
        store
            .gpgconf_command()
            .args(["--list-dirs", "agent-socket"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "timed out running gpgconf".to_string())?
    .map_err(|e| format!("running gpgconf: {e}"))?;
    if !output.status.success() {
        return Err("gpgconf --list-dirs failed".into());
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(PathBuf::from(percent_unescape(raw.trim())))
}

/// `gpgconf` percent-escapes some characters (e.g. `:` as `%3a`) in paths.
fn percent_unescape(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Query `KEYINFO` for each keygrip over the agent socket. `Ok(None)` means
/// the agent isn't running (no socket / connection refused) — which also
/// means nothing is cached.
pub async fn query_socket(
    socket: &Path,
    grips: &[String],
) -> Result<Option<Vec<Option<KeyInfo>>>, String> {
    let stream =
        match tokio::time::timeout(AGENT_TIMEOUT, tokio::net::UnixStream::connect(socket)).await {
            Err(_) => return Err("timed out connecting to gpg-agent".into()),
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Ok(Err(e)) => return Err(format!("connecting to {}: {e}", socket.display())),
            Ok(Ok(s)) => s,
        };
    tokio::time::timeout(AGENT_TIMEOUT, keyinfo(stream, grips))
        .await
        .map_err(|_| "timed out talking to gpg-agent".to_string())?
        .map(Some)
}

/// The Assuan exchange itself, over any stream (unit-tested with an
/// in-memory duplex).
async fn keyinfo<S>(stream: S, grips: &[String]) -> Result<Vec<Option<KeyInfo>>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    read_response(&mut lines, |_| {})
        .await?
        .map_err(|e| format!("agent greeting: {e}"))?;

    let mut out = Vec::with_capacity(grips.len());
    for grip in grips {
        if grip.is_empty() || !grip.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid keygrip {grip:?}"));
        }
        write
            .write_all(format!("KEYINFO {grip}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut info = None;
        let result = read_response(&mut lines, |status| {
            if let Some(rest) = status.strip_prefix("KEYINFO ") {
                info = parse_keyinfo(rest);
            }
        })
        .await?;
        match result {
            Ok(()) => out.push(info),
            Err(code) if code & 0xFFFF == GPG_ERR_NOT_FOUND => out.push(None),
            Err(code) => return Err(format!("gpg-agent KEYINFO failed with error code {code}")),
        }
    }
    let _ = write.write_all(b"BYE\n").await;
    Ok(out)
}

/// Read one response, passing each `S` line's payload to `on_status`.
/// Outer `Err` = protocol/IO failure; inner `Err(code)` = an `ERR` reply.
async fn read_response<R>(
    lines: &mut tokio::io::Lines<R>,
    mut on_status: impl FnMut(&str),
) -> Result<Result<(), u32>, String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|e| e.to_string())?
            .ok_or("gpg-agent closed the connection")?;
        if line == "OK" || line.starts_with("OK ") {
            return Ok(Ok(()));
        }
        if let Some(rest) = line.strip_prefix("ERR ") {
            let code = rest
                .split_whitespace()
                .next()
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            return Ok(Err(code));
        }
        if let Some(status) = line.strip_prefix("S ") {
            on_status(status);
        }
        // `D` data and `#` comment lines carry nothing KEYINFO needs.
    }
}

/// `<grip> <type> <serialno> <idstr> <cached> <protection> …`
fn parse_keyinfo(rest: &str) -> Option<KeyInfo> {
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ch = |i: usize| f.get(i).and_then(|s| s.chars().next());
    Some(KeyInfo {
        kind: ch(1)?,
        cached: f.get(4)? == &"1",
        protection: ch(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRIP_A: &str = "0884A1089616245A3434A61E59FFC6C0A7CE8E60";
    const GRIP_B: &str = "DA8860143E78BEEF72E4C9232E98A0E2C7014E13";

    /// Real GnuPG 2.4 `--with-colons --with-keygrip --list-secret-keys`
    /// output for an ed25519 primary (sign/cert) + cv25519 encryption subkey.
    const COLONS: &str = "\
sec:u:255:22:F8EDFF4A32799371:1794667539:::u:::scESC:::+::ed25519:::0:
fpr:::::::::FD0E69EB3924E8BB360A120DF8EDFF4A32799371:
grp:::::::::0884A1089616245A3434A61E59FFC6C0A7CE8E60:
uid:u::::1794667539::531F78EF29C64193253D34BEFB1E2049F5A4A492::Test <t@example.invalid>::::::::::0:
ssb:u:255:18:D8936DA9BE6E9ED2:1794667539::::::e:::+::cv25519::
fpr:::::::::072C6325E520F5883F6F4889D8936DA9BE6E9ED2:
grp:::::::::DA8860143E78BEEF72E4C9232E98A0E2C7014E13:
";

    #[test]
    fn expired_revoked_disabled_or_invalid_subkeys_are_skipped() {
        for validity in ["e", "r", "d", "i"] {
            let colons = COLONS.replace("ssb:u:", &format!("ssb:{validity}:"));
            assert!(
                parse_encryption_keygrips(&colons).is_empty(),
                "validity {validity} selected"
            );
        }
    }

    #[test]
    fn only_encryption_subkey_grips_are_selected() {
        // The primary's `scESC` has no lowercase `e`: it can't encrypt itself.
        assert_eq!(parse_encryption_keygrips(COLONS), vec![GRIP_B]);
    }

    #[test]
    fn keyinfo_line_parses() {
        // Captured from a real agent after an unlock.
        let rest = format!("{GRIP_B} D - - 1 P - - -");
        assert_eq!(
            parse_keyinfo(&rest),
            Some(KeyInfo {
                kind: 'D',
                cached: true,
                protection: 'P'
            })
        );
        assert_eq!(parse_keyinfo(GRIP_B), None);
    }

    /// Script an agent on one end of an in-memory duplex.
    async fn run_agent(
        replies: &'static str,
        grips: &[&str],
    ) -> Result<Vec<Option<KeyInfo>>, String> {
        let (client, server) = tokio::io::duplex(4096);
        let (sr, mut sw) = tokio::io::split(server);
        let agent = tokio::spawn(async move {
            let mut lines = BufReader::new(sr).lines();
            let mut replies = replies.split("---\n");
            sw.write_all(b"OK Pleased to meet you, process 1\n")
                .await
                .unwrap();
            let mut seen = Vec::new();
            while let Ok(Some(cmd)) = lines.next_line().await {
                if cmd == "BYE" {
                    break;
                }
                seen.push(cmd);
                sw.write_all(replies.next().unwrap().as_bytes())
                    .await
                    .unwrap();
            }
            seen
        });
        let grips: Vec<String> = grips.iter().map(|g| (*g).to_string()).collect();
        let result = keyinfo(client, &grips).await;
        let seen = agent.await.unwrap();
        if result.is_ok() {
            let want: Vec<String> = grips.iter().map(|g| format!("KEYINFO {g}")).collect();
            assert_eq!(seen, want);
        }
        result
    }

    #[tokio::test]
    async fn keyinfo_exchange_reports_cached_and_missing_keys() {
        let replies = "\
# a comment the client must skip
S KEYINFO DA8860143E78BEEF72E4C9232E98A0E2C7014E13 D - - 1 P - - -
OK
---
ERR 67108891 Not found <GPG Agent>
";
        let infos = run_agent(replies, &[GRIP_B, GRIP_A]).await.unwrap();
        assert_eq!(
            infos,
            vec![
                Some(KeyInfo {
                    kind: 'D',
                    cached: true,
                    protection: 'P'
                }),
                None
            ]
        );
        assert_eq!(summarize(&infos), AgentState::Unlocked);
    }

    #[tokio::test]
    async fn unexpected_agent_error_is_surfaced() {
        let err = run_agent("ERR 67108865 General error\n", &[GRIP_B])
            .await
            .unwrap_err();
        assert!(err.contains("67108865"), "{err}");
    }

    #[tokio::test]
    async fn malformed_keygrip_is_never_sent() {
        let (client, server) = tokio::io::duplex(64);
        let (_r, mut w) = tokio::io::split(server);
        w.write_all(b"OK hi\n").await.unwrap();
        let err = keyinfo(client, &["ABC\nKILLAGENT".into()])
            .await
            .unwrap_err();
        assert!(err.contains("invalid keygrip"), "{err}");
    }

    #[tokio::test]
    async fn absent_socket_means_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let got = query_socket(&dir.path().join("S.gpg-agent"), &[GRIP_B.into()])
            .await
            .unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn summarize_precedence() {
        let k = |kind, cached, protection| {
            Some(KeyInfo {
                kind,
                cached,
                protection,
            })
        };
        assert_eq!(summarize(&[k('D', false, 'P')]), AgentState::Locked);
        assert_eq!(summarize(&[k('D', false, 'C')]), AgentState::Unprotected);
        assert_eq!(summarize(&[k('T', false, '-')]), AgentState::Smartcard);
        assert_eq!(summarize(&[None]), AgentState::NoSecretKey);
        assert_eq!(summarize(&[]), AgentState::NoSecretKey);
        assert_eq!(
            summarize(&[k('D', false, 'P'), k('D', true, 'P')]),
            AgentState::Unlocked
        );
    }

    #[test]
    fn gpgconf_paths_are_unescaped() {
        assert_eq!(
            percent_unescape("/x/a%3ab/S.gpg-agent"),
            "/x/a:b/S.gpg-agent"
        );
    }
}
