//! `ajh-tauri agent <verb>` — a thin CLI client over the loopback bridge
//! (issue #1084 PR 1, CLIENT half; the server half is [`super::agent_read`]).
//!
//! Runs as a MODE of the existing `ajh-tauri` binary, selected by an argv
//! sentinel in `main.rs`/`lib::run_agent_cli_if_invoked` — never a second
//! `[[bin]]` (the release upload globs only read `target/release/bundle/**`,
//! so a second binary would ship to nobody). The app must already be
//! running: this sends ONE `agent.query` frame over the same v2
//! mutual-HMAC-authenticated WebSocket the browser extension uses, prints the
//! `agent.result` payload as JSON on stdout, and exits. No DB access, no HTTP
//! server, no new port.
//!
//! ## Why not [`super::native_host::connect_bridge`]
//! That function returns on the first successful WS UPGRADE, before any
//! protocol frame — a port-squatter (anything else listening in
//! [`super::PORT_RANGE`]) would take a native-host relay down; for the CLI it
//! would misreport a squatter as a successful connection with no way to send
//! `agent.query` at all. [`connect_authenticated`] instead drives the FULL
//! v2 handshake (`hello`→`challenge`→`auth`→`auth.ok`) per candidate port and
//! only accepts the one whose **server** proof verifies (see
//! [`super::handshake::verify_server_proof`], added for this client — there
//! was previously no Rust-side implementation of this handshake; the browser
//! extension's lives in TS, `apps/extension/src/lib/bridge.ts`).
//!
//! **This defeats a dumb port squatter (one with no way to answer the
//! challenge), not a RELAYING one** — a local process that transparently
//! proxies bytes between us and the real app would pass the server-proof
//! check too, since it never has to know the token itself, only forward it.
//! The v2 handshake has no channel binding to close that gap; this is a
//! pre-existing, inherent limitation shared with the browser extension's own
//! handshake, not something this client-side change introduces or fixes.
//!
//! ## Exit codes (the process-level contract — see [`run`])
//! - `0` — `agent.result` replied `{"ok":true,...}`; the payload is on stdout.
//! - `1` — `agent.result` replied `{"ok":false,...}` (a server-side refusal:
//!   rate-limited, validation, not-found, autofill off, …) — the payload
//!   (including the fixed-sentinel `error` text) is still on stdout.
//! - `2` — the round trip never completed: bad CLI usage, the app is not
//!   running, or the connection failed for a reason that says nothing about
//!   whether the pairing token itself is valid. A synthesized
//!   `{"ok":false,"resource":…,"error":<fixed sentinel>}` is printed instead
//!   of the (nonexistent) server payload. Never a raw absolute path or an
//!   echoed I/O error string — only fixed sentinels, so this CLI's own stdout
//!   never leaks a path into whatever reads it (an LLM agent's context).
//! - `4` — `call` only (ADR-038 §4, Phase 3): the target is
//!   `Effect::Irreversible` and no `--confirm` was supplied. The reply's
//!   `detail` names WHICH other read command/resource to read the proof
//!   value from and NEVER the value itself — a distinct outcome from a
//!   refusal (exit 2), never collapsed into it.

use std::path::Path;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{ClientRequestBuilder, Message};

use crate::error::{AppError, AppResult};

use super::{
    agent_call, auth, handshake, msg, MAX_FRAME_BYTES, PORT_RANGE, PROTOCOL_VERSION, TOKEN_FILE,
};

type WsStream = tokio_tungstenite::WebSocketStream<TcpStream>;

// ── Error sentinels (the exit-2 `error` field) ──────────────────────────────
// Named once, referenced everywhere they're emitted AND by `help_text()`'s
// own listing — never a second hand-typed copy, so `--help` can't drift from
// what this CLI actually returns (the same anti-drift discipline as
// `agent_read::RESOURCES`).
const ERR_APP_NOT_LOCATED: &str = "app_not_located";
const ERR_PAIRING_TOKEN_UNAVAILABLE: &str = "pairing_token_unavailable";
const ERR_APP_NOT_RUNNING: &str = "app_not_running";
const ERR_PAIRING_REJECTED: &str = "pairing_rejected";
const ERR_CONNECTION_ERROR: &str = "connection_error";
const ERR_RUNTIME_UNAVAILABLE: &str = "runtime_unavailable";
const ERR_CONNECTION_LOST: &str = "connection_lost";
const ERR_TIMEOUT: &str = "timeout";
const ERR_UNSUPPORTED_BY_APP: &str = "unsupported_by_app";
const ERR_USAGE: &str = "usage";

/// `(sentinel, meaning)` — [`help_text`] lists these verbatim.
const ERROR_SENTINELS: &[(&str, &str)] = &[
    (
        ERR_APP_NOT_LOCATED,
        "the app has not written its pointer file yet (needs a newer build, or has never launched)",
    ),
    (
        ERR_PAIRING_TOKEN_UNAVAILABLE,
        "no pairing token on disk yet",
    ),
    (
        ERR_APP_NOT_RUNNING,
        "nothing answered a connect on any candidate port",
    ),
    (
        ERR_PAIRING_REJECTED,
        "every reachable port rejected this token — re-pair from Settings",
    ),
    (
        ERR_CONNECTION_ERROR,
        "a handshake started but failed before authenticating — not evidence of a bad token",
    ),
    (
        ERR_UNSUPPORTED_BY_APP,
        "the running app doesn't understand this verb yet — update it",
    ),
    (
        ERR_TIMEOUT,
        "no reply within the round-trip budget, or the whole invocation ran past its overall deadline",
    ),
    (
        ERR_CONNECTION_LOST,
        "the socket closed or errored mid-round-trip",
    ),
    (ERR_RUNTIME_UNAVAILABLE, "could not start an async runtime"),
    (
        ERR_USAGE,
        "bad CLI usage — see \"detail\" for what was wrong",
    ),
];

/// Wall-clock bound on each individual step of [`attempt_port`] — the raw
/// `TcpStream::connect`, the WS upgrade (`client_async_with_config`), send
/// hello → await challenge, and send auth → await auth.ok. Generous for a
/// loopback round trip; short enough that one hung/squatting port can't
/// stall the whole invocation across [`PORT_RANGE`] (MAJOR fix — security
/// review round 2: `connect`/the WS upgrade used to be the two UNBOUNDED
/// exceptions to that claim — a local process that accepts on a candidate
/// port and never completes the upgrade, including a wedged previous app
/// instance whose accept loop stopped running but whose listener is still
/// bound, parked this fn, and so the whole CLI invocation, forever).
const HANDSHAKE_STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Wall-clock bound on the `agent.query` round trip itself, AFTER
/// authentication — sized above `best-matches`' measured worst case (~12.3s
/// at 4000 found jobs; see `agent_read`'s throttle doc), not the handshake
/// budget above.
const QUERY_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// The WHOLE invocation's outer deadline (MAJOR fix — security review round
/// 2) — wraps [`run_verb`] in [`run`], so no COMBINATION of slow/hung
/// candidate ports can exceed it, even though each individual step above
/// already has its own bound. Derived from the worst *legitimate* sweep, not
/// just one port: up to 5 non-real ports in [`PORT_RANGE`] each maximally
/// stalling both connect and upgrade (2 × [`HANDSHAKE_STEP_TIMEOUT`] = 10s
/// apiece = 50s) before the real app's own port is even reached, PLUS that
/// real port's own worst-case full round trip (2 × `HANDSHAKE_STEP_TIMEOUT`
/// for challenge/auth-ok + [`QUERY_REPLY_TIMEOUT`] for a slow `best-matches`
/// ≈ 40s) — roughly 90s, so this sits right at that sum rather than
/// padding it further: a real hang should surface promptly, not merely
/// "eventually". On expiry [`run`] reports [`ERR_TIMEOUT`] — the same
/// sentinel `send_agent_query_within`'s own post-auth timeout uses, since
/// from the caller's side both mean the identical thing: the CLI gave up
/// after its round-trip budget, whichever phase burned it.
const INVOCATION_TIMEOUT: Duration = Duration::from_secs(90);

// ── argv → verb ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verb {
    BestMatches {
        limit: Option<u64>,
    },
    Job {
        url: String,
    },
    Profile,
    Automations,
    Schema,
    /// Paginated traversal of one autopilot's `found_jobs` (issue #1115) —
    /// `autopilot_get`/`autopilot_list`/`autopilot_best_matches` cannot
    /// enumerate this: the first two are unbounded (every real autopilot
    /// exceeds the MCP bridge's own result cap) and the third is a
    /// cross-autopilot top-N ranking, not a per-autopilot full traversal.
    /// `cursor` is a plain decimal offset into the stored `found_jobs`
    /// order (see `agent_read::resolve_found_jobs`'s doc for why an offset
    /// is sufficient — that order is stable outside a `record_run`/dedup
    /// split).
    FoundJobs {
        autopilot_id: String,
        limit: Option<u64>,
        cursor: Option<String>,
    },
    /// ADR-038 §2's generic dispatch tier (`agent call <namespace>:<command>
    /// [--input '<json>'] [--confirm '<value>']`) — a SEPARATE wire frame
    /// (`agent.call`, never `agent.query`) and reply shape (`dispatched`,
    /// never `ok`); see [`Verb::wire_type`]/[`Verb::reply_type`] and
    /// `run_verb`'s own dispatched-vs-ok branch below. `confirm` is the
    /// Phase 3 ceremony's proof value for an `Effect::Irreversible` command
    /// — `None` for every other row, and never logged/echoed anywhere on
    /// this client (see `parse_call`'s own doc).
    Call {
        namespace: String,
        command: String,
        input: Value,
        confirm: Option<String>,
    },
}

impl Verb {
    fn resource_name(&self) -> &'static str {
        match self {
            Verb::BestMatches { .. } => "best-matches",
            Verb::Job { .. } => "job",
            Verb::Profile => "profile",
            Verb::Automations => "automations",
            Verb::Schema => "schema",
            Verb::FoundJobs { .. } => "found-jobs",
            Verb::Call { .. } => "call",
        }
    }

    /// The outbound frame's `type` — every curated verb sends `agent.query`;
    /// [`Verb::Call`] sends the generic tier's own `agent.call` instead (two
    /// visibly different grammars, ADR-038 §2's own framing).
    fn wire_type(&self) -> &'static str {
        match self {
            Verb::Call { .. } => msg::AGENT_CALL,
            _ => msg::AGENT_QUERY,
        }
    }

    /// The expected reply frame's `type` — the mirror of [`Self::wire_type`].
    fn reply_type(&self) -> &'static str {
        match self {
            Verb::Call { .. } => msg::AGENT_CALL_RESULT,
            _ => msg::AGENT_RESULT,
        }
    }

    /// The outbound frame's `payload` object for this verb.
    fn payload(&self) -> Value {
        match self {
            Verb::BestMatches { limit } => {
                let mut p = json!({ "resource": self.resource_name() });
                if let Some(limit) = limit {
                    p["limit"] = json!(limit);
                }
                p
            }
            Verb::Job { url } => json!({ "resource": self.resource_name(), "url": url }),
            Verb::Profile | Verb::Automations | Verb::Schema => {
                json!({ "resource": self.resource_name() })
            }
            Verb::FoundJobs {
                autopilot_id,
                limit,
                cursor,
            } => {
                let mut p =
                    json!({ "resource": self.resource_name(), "autopilotId": autopilot_id });
                if let Some(limit) = limit {
                    p["limit"] = json!(limit);
                }
                if let Some(cursor) = cursor {
                    p["cursor"] = json!(cursor);
                }
                p
            }
            Verb::Call {
                namespace,
                command,
                input,
                confirm,
            } => {
                let mut p = json!({ "namespace": namespace, "command": command, "input": input });
                if let Some(confirm) = confirm {
                    p["confirm"] = json!(confirm);
                }
                p
            }
        }
    }
}

/// One verb's `--help` metadata. [`VERB_TABLE`] is `parse_verb`'s own
/// canonical name list (never a second hand-typed one — see [`help_text`]
/// and `parse_verb`'s "unknown verb" branch below, both of which read from
/// this SAME array) so the CLI's usage text cannot silently drift from what
/// it actually parses (LOW fix — security review, folded into this same
/// verb table so it can't recur here either).
struct VerbHelp {
    name: &'static str,
    args: &'static str,
    returns: &'static str,
}

const VERB_TABLE: &[VerbHelp] = &[
    VerbHelp {
        name: "best-matches",
        args: "[--limit <n>]",
        returns: "the strongest jobs across every autopilot (default 20, max 50)",
    },
    VerbHelp {
        name: "job",
        args: "<url>",
        returns: "full detail for one posting",
    },
    VerbHelp {
        name: "profile",
        args: "",
        returns:
            "contact-profile fields for autofill (same consent gate as the extension's profile.get)",
    },
    VerbHelp {
        name: "automations",
        args: "",
        returns: "every autopilot and its status",
    },
    VerbHelp {
        name: "schema",
        args: "",
        returns: "this resource list, as machine-readable JSON",
    },
    VerbHelp {
        name: "found-jobs",
        args: "<autopilotId> [--limit <n>] [--cursor <c>]",
        returns: "one page of an autopilot's complete found-jobs list (default/max limit and the \
                  cursor format are documented on `agent_read::resolve_found_jobs`); repeat with \
                  the returned cursor until it comes back null to traverse the whole list",
    },
    VerbHelp {
        name: "call",
        args: "<namespace>:<command> [--input '<json>'] [--confirm '<value>']",
        returns: "ADR-038 §2's generic dispatch tier — Read/Reversible commands dispatch \
                  directly; an Irreversible command needs --confirm '<value>' (a proof read \
                  from ANOTHER command, named but never disclosed by a --confirm-less call — \
                  exit 4); NotExposed always refuses (see `agent schema`, the MCP `commands` \
                  tool, or policy.rs for the full table)",
    },
];

fn verb_names_joined() -> String {
    VERB_TABLE
        .iter()
        .map(|v| v.name)
        .collect::<Vec<_>>()
        .join("|")
}

/// Parse `args` (excludes the program name AND the `agent` sentinel itself —
/// e.g. `["best-matches", "--limit", "10"]`). `--help`/`-h`/bare `help` are
/// intercepted by [`run`] BEFORE this is ever called, so this only ever sees
/// a real (or invalid) verb attempt. `AppError::Validation` per
/// `rust-standards`' R6 (no stringly-typed `Result<_, String>` outside
/// `error.rs`), even for this process-local, never-IPC-round-tripped parse.
fn parse_verb(args: &[String]) -> AppResult<Verb> {
    match args.first().map(String::as_str) {
        Some("best-matches") => parse_best_matches(&args[1..]),
        Some("job") => {
            let url = args
                .get(1)
                .map(String::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::Validation("job requires a <url> argument".to_string()))?;
            Ok(Verb::Job {
                url: url.to_string(),
            })
        }
        Some("profile") => Ok(Verb::Profile),
        Some("automations") => Ok(Verb::Automations),
        Some("schema") => Ok(Verb::Schema),
        Some("found-jobs") => parse_found_jobs(&args[1..]),
        Some("call") => parse_call(&args[1..]),
        // Never echoes the typed token (LOW fix — security review): argv can
        // carry a path/username, and this reply lands in an agent transcript
        // — list the allowed verbs instead of the one that failed.
        Some(_) => Err(AppError::Validation(format!(
            "unknown verb (run `ajh-tauri agent --help`; expected one of: {})",
            verb_names_joined()
        ))),
        None => Err(AppError::Validation(format!(
            "missing verb (run `ajh-tauri agent --help`; expected one of: {})",
            verb_names_joined()
        ))),
    }
}

fn parse_best_matches(rest: &[String]) -> AppResult<Verb> {
    let mut limit = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--limit" => {
                let raw = rest
                    .get(i + 1)
                    .ok_or_else(|| AppError::Validation("--limit requires a value".to_string()))?;
                limit = Some(raw.parse::<u64>().map_err(|_| {
                    AppError::Validation("--limit must be a non-negative integer".to_string())
                })?);
                i += 2;
            }
            // Never echoes the typed token (MINOR fix — same reasoning as
            // the unknown-verb branch above and pinned by the same kind of
            // test): argv can carry a path/username, and this reply lands
            // in an agent transcript — name the flag this verb accepts
            // instead of the one that failed.
            _ => {
                return Err(AppError::Validation(
                    "unknown argument (expected: --limit)".to_string(),
                ))
            }
        }
    }
    Ok(Verb::BestMatches { limit })
}

/// Parse `found-jobs`' own args: `<autopilotId> [--limit <n>] [--cursor <c>]`.
/// Mirrors [`parse_best_matches`]'s flag-parsing loop, plus the one
/// positional argument every other multi-arg verb here (`job`) also takes
/// first. Never echoes an unknown flag's raw token (same reasoning as
/// [`parse_best_matches`]'s own comment).
fn parse_found_jobs(rest: &[String]) -> AppResult<Verb> {
    let autopilot_id = rest
        .first()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Validation("found-jobs requires an <autopilotId> argument".to_string())
        })?
        .to_string();

    let mut limit = None;
    let mut cursor = None;
    let mut i = 1;
    while i < rest.len() {
        match rest[i].as_str() {
            "--limit" => {
                let raw = rest
                    .get(i + 1)
                    .ok_or_else(|| AppError::Validation("--limit requires a value".to_string()))?;
                limit = Some(raw.parse::<u64>().map_err(|_| {
                    AppError::Validation("--limit must be a non-negative integer".to_string())
                })?);
                i += 2;
            }
            "--cursor" => {
                let raw = rest
                    .get(i + 1)
                    .ok_or_else(|| AppError::Validation("--cursor requires a value".to_string()))?;
                cursor = Some(raw.to_string());
                i += 2;
            }
            _ => {
                return Err(AppError::Validation(
                    "unknown argument (expected: --limit, --cursor)".to_string(),
                ))
            }
        }
    }
    Ok(Verb::FoundJobs {
        autopilot_id,
        limit,
        cursor,
    })
}

/// Parse `call`'s own args: `<namespace>:<command> [--input '<json>']
/// [--confirm '<value>']`. Both target-parsing failure modes are pure ARGV
/// shape — no policy-table lookup, no network — so they resolve the SAME way
/// `--help` does: without the app running. Whether `<namespace>:<command>`
/// names a real, dispatchable command (and which class it is) is decided
/// server-side (`agent_call::dispatch`), never guessed here — this fn only
/// rejects a token that couldn't possibly be one.
///
/// `--confirm`'s raw value is NEVER echoed in any error here, and is carried
/// only as far as [`Verb::payload`] — this client never logs it, never
/// prints it outside the one frame it belongs on (path privacy AND ADR-038
/// §4's own "the caller's own data" rule apply equally to this flag).
fn parse_call(rest: &[String]) -> AppResult<Verb> {
    let target = rest
        .first()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Validation("call requires a <namespace>:<command> argument".to_string())
        })?;
    let (namespace, command) = target
        .split_once(':')
        .filter(|(n, c)| !n.is_empty() && !c.is_empty())
        .ok_or_else(|| {
            AppError::Validation(
                "call's first argument must be <namespace>:<command> (see `agent schema` or the \
                 MCP `commands` tool)"
                    .to_string(),
            )
        })?;

    let mut input = json!({});
    let mut confirm = None;
    let mut i = 1;
    while i < rest.len() {
        match rest[i].as_str() {
            "--input" => {
                let raw = rest
                    .get(i + 1)
                    .ok_or_else(|| AppError::Validation("--input requires a value".to_string()))?;
                // Never echoes `raw` (path privacy — the value may carry a
                // path or other sensitive content the caller typed).
                let parsed: Value = serde_json::from_str(raw)
                    .map_err(|_| AppError::Validation("--input must be valid JSON".to_string()))?;
                if !parsed.is_object() {
                    return Err(AppError::Validation(
                        "--input must be a JSON object".to_string(),
                    ));
                }
                input = parsed;
                i += 2;
            }
            "--confirm" => {
                let raw = rest.get(i + 1).ok_or_else(|| {
                    AppError::Validation("--confirm requires a value".to_string())
                })?;
                // Never echoed anywhere — this IS the ceremony's proof
                // value (ADR-038 §4).
                confirm = Some(raw.to_string());
                i += 2;
            }
            _ => {
                return Err(AppError::Validation(
                    "unknown argument (expected: --input, --confirm)".to_string(),
                ))
            }
        }
    }
    Ok(Verb::Call {
        namespace: namespace.to_string(),
        command: command.to_string(),
        input,
        confirm,
    })
}

// ── agent-CLI pointer (written by `super::register::write_agent_pointer`) ──

#[derive(Debug, Deserialize)]
struct AgentPointer {
    #[serde(rename = "dataDir")]
    data_dir: String,
}

/// Read + parse the pointer file, or `None` on any I/O/parse failure. The
/// caller reports this as `app_not_located`, NOT `app_not_running`: the app
/// may well be running and simply not have written a pointer yet (it is
/// written on launch, so a build predating it leaves none), and conflating
/// the two sends anyone debugging this to look at the wrong thing — it did
/// exactly that during this feature's own end-to-end verification.
/// Never logs/echoes the path itself (path privacy).
fn read_agent_pointer() -> Option<AgentPointer> {
    let path = crate::platform::config::agent_pointer_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Reject a `dataDir` that is not an absolute LOCAL path (MEDIUM fix —
/// security review). The pointer file is written by the app itself, but this
/// CLI treats its `dataDir` value as arriving from disk and joins it
/// unvalidated — a UNC path (`\\attacker.example.com\share`) turns the
/// read below into an outbound SMB/WebDAV session on Windows, leaking NTLM
/// credentials to whatever host it names: a network primitive smuggled
/// through a file read, on a path `tests/egress.rs`'s allowlist does not
/// cover. Windows treats `/` and `\` interchangeably as path separators, so
/// the two leading bytes are checked as EITHER separator in EITHER
/// combination (`\\`, `//`, `\/`, `/\`) — confirmed against `ntpath` (a
/// faithful model of Windows path parsing) that a mixed-separator UNC root
/// still parses as absolute UNC and, before this fix, passed a check that
/// only matched the two same-separator prefixes literally (a MEDIUM fix,
/// security review round 2 — a straight string-prefix match doesn't survive
/// Windows' separator equivalence). Plain byte checks, so this runs before
/// ANY filesystem call.
fn is_safe_local_data_dir(data_dir: &str) -> bool {
    let bytes = data_dir.as_bytes();
    let is_sep = |b: Option<&u8>| matches!(b, Some(b'\\') | Some(b'/'));
    if is_sep(bytes.first()) && is_sep(bytes.get(1)) {
        return false;
    }
    Path::new(data_dir).is_absolute()
}

/// Read the persisted pairing token from `data_dir` — the exact file
/// [`super::persist::persist_token`] writes, read the same way
/// [`super::persist::load_or_create_token`] does (trimmed, empty ⇒ absent).
/// `None` (never a filesystem read) for a `dataDir` [`is_safe_local_data_dir`]
/// rejects — the caller reports the same `pairing_token_unavailable` sentinel
/// as any other absent/unreadable token.
fn read_pairing_token(data_dir: &str) -> Option<String> {
    if !is_safe_local_data_dir(data_dir) {
        return None;
    }
    let text = std::fs::read_to_string(Path::new(data_dir).join(TOKEN_FILE)).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

// ── v2 mutual handshake — the client half (see the module doc) ─────────────

/// Build the `hello` frame (handshake step 1).
fn build_hello(client_nonce: &str) -> String {
    json!({
        "type": msg::HELLO,
        "reqId": "cli-hello",
        "payload": { "protocol": PROTOCOL_VERSION, "clientNonce": client_nonce },
    })
    .to_string()
}

/// Extract `serverNonce` from a `challenge` reply, validating its shape
/// (mirrors the server's own `is_valid_nonce` check on the client nonce).
/// `None` for anything that isn't a well-formed challenge.
fn parse_challenge(v: &Value) -> Option<String> {
    if v.get("type").and_then(Value::as_str) != Some(msg::CHALLENGE) {
        return None;
    }
    let nonce = v.get("payload")?.get("serverNonce")?.as_str()?;
    handshake::is_valid_nonce(nonce).then(|| nonce.to_string())
}

/// Build the `auth` frame (handshake step 3) carrying the client's proof.
fn build_auth(proof: &str) -> String {
    json!({
        "type": msg::AUTH,
        "reqId": "cli-auth",
        "payload": { "proof": proof },
    })
    .to_string()
}

/// Extract `serverProof` from an `auth.ok` reply. `None` for anything else
/// (a different type, a missing/non-string field).
fn parse_auth_ok(v: &Value) -> Option<String> {
    if v.get("type").and_then(Value::as_str) != Some(msg::AUTH_OK) {
        return None;
    }
    Some(v.get("payload")?.get("serverProof")?.as_str()?.to_string())
}

/// Read the next parseable JSON text frame within `dur`, silently skipping
/// ping/pong control frames. `None` on timeout, a transport error, a close,
/// or non-JSON content — every one of those collapses to the same "this port
/// gave us nothing usable" signal for the caller.
///
/// `dur` is a single deadline for the WHOLE call, computed once on entry —
/// NOT re-armed on every loop iteration. A peer that emits a ping/pong (or
/// any other non-Text/Binary/Close frame) faster than `dur` would otherwise
/// keep resetting `timeout`'s clock forever and this call — and everything
/// waiting on it, including [`send_agent_query`]'s own shrinking
/// `remaining` budget, which never gets a chance to re-run while this loop
/// is stuck — would never return.
async fn next_json(ws: &mut WsStream, dur: Duration) -> Option<Value> {
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let msg = match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => return None,
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8(b.to_vec()).ok()?,
            Message::Close(_) => return None,
            _ => continue,
        };
        return serde_json::from_str(&text).ok();
    }
}

/// One candidate port's outcome, coarse enough to drive
/// [`classify_pairing_failure`] without leaking WHICH failure mode occurred
/// (see that function's doc for why only three buckets exist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortOutcome {
    /// Nothing answered a TCP connect / WS upgrade on this port.
    NoUpgrade,
    /// The WS upgrade succeeded, but the connection ended BEFORE we ever sent
    /// our `auth` proof (an I/O error, a timeout, or a malformed/missing
    /// challenge). NOT evidence about our own token — issue #1084 PR1's own
    /// decision: "a crash between challenge and auth is not a pairing
    /// failure."
    PreAuthError,
    /// We sent `auth{proof}`, and the port failed to answer with a
    /// VERIFYING `auth.ok` — silence, a close, a malformed reply, or a
    /// `serverProof` that failed constant-time verification. Folded into one
    /// bucket because the server's own failed-proof path is, by design, a
    /// silent close indistinguishable from a crash (see
    /// `extension_bridge::advance_auth`'s doc) — once we have committed our
    /// proof, any non-verifying outcome is attributed to proof rejection.
    ProofRejected,
}

/// Drive the full handshake against one port. `Ok` only once the SERVER's
/// proof has verified (mutual auth complete); every other case reports
/// [`PortOutcome`] instead of the (now-dropped) socket.
///
/// Both `connect` and the WS upgrade below are wrapped in
/// [`HANDSHAKE_STEP_TIMEOUT`] (MAJOR fix — security review round 2): before
/// this fix they were the two UNBOUNDED steps in an otherwise fully-budgeted
/// function — a local process that accepts a connection on this port and
/// never completes either step (a wedged previous app instance whose
/// listener is still bound but whose accept loop stopped running is exactly
/// this: `connect` succeeds instantly off the kernel's own backlog, then the
/// upgrade read waits forever for a reply nothing will ever send) parked
/// this fn, and so [`connect_authenticated`]'s whole port loop, forever. Both
/// timeout outcomes fold into [`PortOutcome::NoUpgrade`] — "nothing usable
/// answered" is exactly what that variant already means, whether the cause
/// was a refused connect, a rejected upgrade, or one of these now-bounded
/// hangs.
async fn attempt_port(port: u16, token: &str) -> Result<WsStream, PortOutcome> {
    let tcp = timeout(
        HANDSHAKE_STEP_TIMEOUT,
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map_err(|_| PortOutcome::NoUpgrade)?
    .map_err(|_| PortOutcome::NoUpgrade)?;
    let uri = format!("ws://127.0.0.1:{port}/")
        .parse()
        .map_err(|_| PortOutcome::NoUpgrade)?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    // Its OWN sentinel Origin (finding #5, security review) — distinct from
    // the native host's, so the server can tell "the CLI" apart from "the
    // browser extension arriving via the native-host relay" and gate
    // `agent.query` on it (see `auth::AGENT_CLI_ORIGIN`'s doc for exactly
    // what this label does and doesn't defend against). The origin check
    // remains defense-in-depth only; the mutual HMAC handshake below is the
    // real boundary.
    let request = ClientRequestBuilder::new(uri).with_header("Origin", auth::AGENT_CLI_ORIGIN);
    let (mut ws, _resp) = timeout(
        HANDSHAKE_STEP_TIMEOUT,
        tokio_tungstenite::client_async_with_config(request, tcp, Some(config)),
    )
    .await
    .map_err(|_| PortOutcome::NoUpgrade)?
    .map_err(|_| PortOutcome::NoUpgrade)?;

    let client_nonce = handshake::new_nonce();
    if ws
        .send(Message::text(build_hello(&client_nonce)))
        .await
        .is_err()
    {
        return Err(PortOutcome::PreAuthError);
    }
    let server_nonce = next_json(&mut ws, HANDSHAKE_STEP_TIMEOUT)
        .await
        .and_then(|v| parse_challenge(&v))
        .ok_or(PortOutcome::PreAuthError)?;

    let proof = handshake::client_proof(token, &server_nonce, &client_nonce);
    if ws.send(Message::text(build_auth(&proof))).await.is_err() {
        // Delivery itself is unconfirmed — we never received anything that
        // could be a rejection signal, so this is NOT a proof rejection.
        return Err(PortOutcome::PreAuthError);
    }

    let server_proof = next_json(&mut ws, HANDSHAKE_STEP_TIMEOUT)
        .await
        .and_then(|v| parse_auth_ok(&v));
    match server_proof {
        Some(proof)
            if handshake::verify_server_proof(token, &server_nonce, &client_nonce, &proof) =>
        {
            Ok(ws)
        }
        _ => Err(PortOutcome::ProofRejected),
    }
}

/// Why every candidate port fell short of authenticating, folded into ONE
/// process-level verdict — see the exit-code table in the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingFailure {
    /// No port in [`PORT_RANGE`] answered a TCP connect/WS upgrade at all.
    AppNotRunning,
    /// Every port that upgraded also rejected our proof — the persisted
    /// pairing token is stale (this CLI process's copy, read fresh from the
    /// token file every invocation, no longer matches the app's).
    PairingRejected,
    /// At least one upgraded port failed BEFORE the proof-rejection point —
    /// inconclusive, and specifically NOT evidence the token is wrong (see
    /// [`PortOutcome::PreAuthError`]'s doc).
    ConnectionError,
}

/// Pure aggregation over this invocation's [`PortOutcome`]s — kept separate
/// from the async I/O in [`connect_authenticated`] so it is directly
/// unit-testable. Only counts ports that actually upgraded ("every port
/// completed an upgrade and every one rejected the proof" from a range where,
/// in practice, only ONE port is ever bound — the rest are simply absent).
fn classify_pairing_failure(outcomes: &[PortOutcome]) -> PairingFailure {
    let saw_upgrade = outcomes.iter().any(|o| *o != PortOutcome::NoUpgrade);
    let saw_pre_auth_error = outcomes.contains(&PortOutcome::PreAuthError);
    if !saw_upgrade {
        PairingFailure::AppNotRunning
    } else if saw_pre_auth_error {
        PairingFailure::ConnectionError
    } else {
        PairingFailure::PairingRejected
    }
}

/// For each port in [`PORT_RANGE`], drive the full handshake and accept the
/// first one whose server proof verifies. See the module doc for why this
/// must not reuse [`super::native_host::connect_bridge`].
async fn connect_authenticated(token: &str) -> Result<WsStream, PairingFailure> {
    let mut outcomes = Vec::new();
    for port in PORT_RANGE {
        match attempt_port(port, token).await {
            Ok(ws) => return Ok(ws),
            Err(outcome) => outcomes.push(outcome),
        }
    }
    Err(classify_pairing_failure(&outcomes))
}

// ── agent.query round trip ──────────────────────────────────────────────────

/// [`send_agent_query`], but with the overall budget as an explicit
/// parameter — directly unit-testable against a real (but fast) loopback
/// server without waiting out the real 30s [`QUERY_REPLY_TIMEOUT`].
/// Production always goes through the convenience wrapper below.
///
/// Waits for the matching `agent.result` (by `reqId`), within `budget`
/// overall. A `token.revoked` seen instead (the pairing was rotated
/// mid-session) is reported distinctly rather than left to time out. Any
/// OTHER frame carrying a DIFFERENT `reqId` is ignored — a fresh, one-shot
/// connection should never see one, but ignoring rather than failing on it
/// costs nothing and is more robust to a future additive frame.
async fn send_agent_query_within(
    mut ws: WsStream,
    verb: &Verb,
    budget: Duration,
) -> Result<Value, &'static str> {
    let req_id = uuid::Uuid::new_v4().to_string();
    let frame = json!({
        "type": verb.wire_type(),
        "reqId": req_id,
        "payload": verb.payload(),
    })
    .to_string();
    if ws.send(Message::text(frame)).await.is_err() {
        return Err(ERR_CONNECTION_LOST);
    }

    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ERR_TIMEOUT);
        }
        let Some(v) = next_json(&mut ws, remaining).await else {
            // `next_json` returning `None` is EITHER a genuine timeout (it
            // burned the whole `remaining` budget waiting) OR a real
            // transport failure (a close/IO error/malformed frame arriving
            // well BEFORE the deadline) — the two used to collapse into one
            // `connection_lost`, so a real 30s round-trip timeout (measured
            // against v0.144.0: `agent schema` returned `connection_lost`
            // after burning the full budget) misreported as a transport
            // failure instead. Distinguish by checking the clock: only a
            // call that actually reached the deadline is a timeout (same
            // defect class as `6bdd6785` — a definite outcome misreported as
            // something else).
            return if Instant::now() >= deadline {
                Err(ERR_TIMEOUT)
            } else {
                Err(ERR_CONNECTION_LOST)
            };
        };
        match v.get("type").and_then(Value::as_str) {
            Some(t)
                if t == verb.reply_type()
                    && v.get("reqId").and_then(Value::as_str) == Some(req_id.as_str()) =>
            {
                return v.get("payload").cloned().ok_or(ERR_CONNECTION_LOST);
            }
            Some(t) if t == msg::TOKEN_REVOKED => return Err(ERR_PAIRING_REJECTED),
            _ if v.get("reqId").and_then(Value::as_str) == Some(req_id.as_str()) => {
                // Any OTHER frame carrying OUR OWN reqId is precisely
                // detectable: an app that doesn't understand this verb's
                // wire type replies via `advance_authenticated`'s "unknown
                // message type" fallback, echoing this exact reqId on an
                // `import.result` envelope. Fail fast instead of waiting out
                // the full `QUERY_REPLY_TIMEOUT` for a reply that will never
                // arrive.
                return Err(ERR_UNSUPPORTED_BY_APP);
            }
            _ => continue,
        }
    }
}

/// Send one `agent.query`, budgeted at [`QUERY_REPLY_TIMEOUT`].
async fn send_agent_query(ws: WsStream, verb: &Verb) -> Result<Value, &'static str> {
    send_agent_query_within(ws, verb, QUERY_REPLY_TIMEOUT).await
}

// ── entrypoint + output ─────────────────────────────────────────────────────

fn pairing_failure_sentinel(f: PairingFailure) -> &'static str {
    match f {
        PairingFailure::AppNotRunning => ERR_APP_NOT_RUNNING,
        PairingFailure::PairingRejected => ERR_PAIRING_REJECTED,
        PairingFailure::ConnectionError => ERR_CONNECTION_ERROR,
    }
}

/// The exit-2 usage-error body — pulled out as its own pure fn (MINOR fix —
/// security review round 2) so it's directly unit-testable, and so its shape
/// stays byte-for-byte in sync with [`emit_cli_error`]'s: both always carry
/// `resource` (this one `null` — no `Verb` exists yet at the point a usage
/// error is raised), matching the module doc's exit-2 table. `detail` is
/// this branch's own extra, human/agent-useful context, not part of the
/// shape the doc pins.
fn usage_error_value(detail: &str) -> Value {
    json!({ "ok": false, "resource": Value::Null, "error": ERR_USAGE, "detail": detail })
}

/// Print a synthesized CLI-level error (exit 2) and return that code. Never
/// echoes a path or a raw I/O error string — only fixed sentinels (see the
/// module doc's exit-code table).
fn emit_cli_error(resource: Option<&str>, sentinel: &str) -> i32 {
    println!(
        "{}",
        json!({ "ok": false, "resource": resource, "error": sentinel })
    );
    2
}

/// The bridge round trip common to every verb, minus the CLI's own
/// stdout/exit-code translation: pointer → token → [`connect_authenticated`]
/// → [`send_agent_query`]. [`run_verb`] is now just this plus its own
/// `println!`/[`exit_code_for_reply`] wrapping; [`mcp`] calls this directly
/// instead of `run_verb`, since the MCP wire owns its own stdout discipline
/// (a single `writeln!` site — see that module's doc) and must never emit
/// this fn's sibling's bare `println!` envelope.
async fn query(verb: &Verb) -> Result<Value, &'static str> {
    let pointer = read_agent_pointer().ok_or(ERR_APP_NOT_LOCATED)?;
    let token = read_pairing_token(&pointer.data_dir).ok_or(ERR_PAIRING_TOKEN_UNAVAILABLE)?;
    let ws = connect_authenticated(&token)
        .await
        .map_err(pairing_failure_sentinel)?;
    send_agent_query(ws, verb).await
}

async fn run_verb(verb: Verb) -> i32 {
    let resource = verb.resource_name();
    match query(&verb).await {
        Ok(payload) => {
            println!("{payload}");
            exit_code_for_reply(&verb, &payload)
        }
        Err(sentinel) => emit_cli_error(Some(resource), sentinel),
    }
}

/// The reply's own truth field decides the exit code, and it differs BY
/// TIER (ADR-038 §2/§5): the curated tier keeps a truthful `ok` (0 on
/// `true`, 1 — "the app replied with a refusal" — on `false`). The generic
/// `call` tier never claims `ok`; its `dispatched` means only "did
/// `Webview::on_message` run", so `false` there is normally a REFUSAL BEFORE
/// dispatch (unknown command, wrong effect class, rate-limited) — the SAME
/// class as a usage error, hence exit 2, not 1. ONE `dispatched:false` cause
/// is its own distinct exit code (ADR-038 §4, Phase 3): an `Irreversible`
/// command called with no `--confirm` is `confirmation_required`, which
/// exits 4 rather than 2 — "needs confirmation" is a different outcome from
/// a refusal, never collapsed into it (the payload's own `error` field is
/// what names every cause; this fn only routes the ONE that gets a
/// different process exit code).
fn exit_code_for_reply(verb: &Verb, payload: &Value) -> i32 {
    match verb {
        Verb::Call { .. } => {
            if payload.get("error").and_then(Value::as_str)
                == Some(agent_call::ERR_CONFIRMATION_REQUIRED)
            {
                return 4;
            }
            let dispatched = payload
                .get("dispatched")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if dispatched {
                0
            } else {
                2
            }
        }
        _ => {
            let ok = payload.get("ok").and_then(Value::as_bool).unwrap_or(false);
            i32::from(!ok)
        }
    }
}

/// Whether `args`' first token requests help. `-h`/`--help` are checked
/// anywhere the flag would normally sit as the FIRST argument (this CLI has
/// no other flags before a verb); a bare `help` verb is also accepted.
fn is_help_request(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("--help") | Some("-h") | Some("help")
    )
}

/// Whether `args`' first token requests the MCP server mode
/// (`agent mcp [--allow-irreversible]`) — a MODE like `--help`, never a
/// `Verb`/`VERB_TABLE` row (see [`mcp`]'s own module doc for why).
fn is_mcp_mode(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some("mcp")
}

/// Human-readable usage text — derived ENTIRELY from [`VERB_TABLE`] and
/// [`ERROR_SENTINELS`], never a hand-typed second copy of either (see both
/// constants' own docs). Pure, allocation-only: no `AppHandle`, no pointer
/// file, no token, no socket — safe to print with the app not running at all
/// (the owner's hard requirement for `--help`).
fn help_text() -> String {
    let mut out = String::from(
        "ajh-tauri agent <verb> [args]\n\n\
         A thin CLI client over the AI Job Hunter desktop app's loopback bridge.\n\
         The desktop app must already be running for any verb below EXCEPT --help.\n\n\
         VERBS:\n",
    );
    for v in VERB_TABLE {
        out.push_str(&format!("  {:<16}{:<16}{}\n", v.name, v.args, v.returns));
    }
    out.push_str(
        "  --help, -h, help                Show this help and exit (works even if the app is not running).\n\
         \x20\x20mcp [--allow-reversible] [--allow-irreversible]\n\
                                  Run as an MCP (Model Context Protocol) stdio server for Claude \
           Code/Codex; read tier + `commands` only by default, --allow-reversible adds \
           mutating-but-undoable tools, --allow-irreversible adds the rest (implies \
           --allow-reversible). `agent mcp --help` shows its own usage.\n\n\
         EXIT CODES:\n\
         \x20 0   Success — the reply is printed as JSON on stdout.\n\
         \x20 1   The app replied with a refusal (rate-limited, validation, not found, autofill off, ...) \
           — still printed as JSON on stdout.\n\
         \x20 2   The round trip never completed, or usage was invalid — see \"error\" below.\n\
         \x20 4   `call` only: an Effect::Irreversible command needs --confirm '<value>' — the \
           reply's \"detail\" names which OTHER read command/resource to read the proof from, \
           and never the value itself (ADR-038 §4).\n\n\
         ERROR SENTINELS (the \"error\" field on an exit-2 reply):\n",
    );
    for (sentinel, meaning) in ERROR_SENTINELS {
        out.push_str(&format!("  {sentinel:<26}{meaning}\n"));
    }
    out
}

/// Race `fut` (in production, [`run_verb`]'s own future) against `budget` —
/// [`run`]'s outer, WHOLE-INVOCATION deadline (MAJOR fix — security review
/// round 2; see [`INVOCATION_TIMEOUT`]'s own doc for why this exists
/// alongside, not instead of, the per-step timeouts already inside
/// [`attempt_port`]/[`send_agent_query_within`]). `resource` is a plain
/// `&str` rather than a `Verb` so the caller can hand this a `Verb`'s
/// `resource_name()` BEFORE moving the `Verb` itself into `fut` — a `Verb`
/// doesn't survive being consumed by the future this races.
///
/// Generic over `F` (rather than `run_verb`'s own concrete future) so this
/// race's OUTCOME is directly unit-testable against a controllable budget
/// and a controllable inner future, without a live pointer file/token/socket
/// and without waiting out the real [`INVOCATION_TIMEOUT`] — mirrors
/// [`send_agent_query_within`]'s existing "explicit budget parameter, prod
/// wraps it" pattern one section up.
async fn run_verb_within<F>(resource: &str, budget: Duration, fut: F) -> i32
where
    F: std::future::Future<Output = i32>,
{
    match timeout(budget, fut).await {
        Ok(code) => code,
        Err(_) => emit_cli_error(Some(resource), ERR_TIMEOUT),
    }
}

/// `ajh-tauri agent <verb>` entrypoint. `args` excludes the program name AND
/// the `agent` sentinel itself. Called from `lib::run_agent_cli_if_invoked`,
/// itself called from `main()` BELOW the native-host short-circuit and ABOVE
/// `ajh_tauri::run()` — see that function's doc for why the ordering matters.
/// Builds its OWN current-thread Tokio runtime (mirrors
/// [`super::native_host::run`]): this path runs before Tauri boots, so there
/// is no ambient reactor. Never panics out.
pub fn run(args: &[String]) -> i32 {
    // MUST run first — `--help` is the single most likely command a human
    // types interactively on Windows, precisely the NULL-stdout case this
    // probe exists for (`platform::windows_console`'s own doc).
    crate::platform::windows_console::ensure_console_output();

    if is_help_request(args) {
        // No pointer, no token, no socket, no network — pure local text, per
        // the owner's requirement that `--help` work with the app NOT
        // running.
        println!("{}", help_text());
        return 0;
    }
    if is_mcp_mode(args) {
        // A MODE, not a `Verb` — intercepted before `parse_verb` exactly
        // like `--help` above, so it forces no nonsense `wire_type`/
        // `payload` match arms and touches neither `VERB_TABLE` nor its own
        // drift tests (see `mcp`'s own module doc).
        return mcp::run(&args[1..]);
    }
    if args.is_empty() {
        // A bare `ajh-tauri agent` is far more likely a human looking for
        // guidance than a scripted caller depending on today's terse JSON
        // usage error, so it gets the SAME help text `--help` prints — to
        // stderr (this is still an error exit), never stdout, so a script
        // that only reads stdout for the JSON reply sees nothing new.
        eprintln!("{}", help_text());
        return 2;
    }

    let verb = match parse_verb(args) {
        Ok(v) => v,
        Err(e) => {
            // See `usage_error_value`'s doc (MINOR fix — security review
            // round 2): this branch runs before a `Verb` exists, but the
            // exit-2 shape must still carry `resource` (null here), the same
            // as every other exit-2 reply on this surface.
            println!("{}", usage_error_value(&e.to_string()));
            return 2;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return emit_cli_error(Some(verb.resource_name()), ERR_RUNTIME_UNAVAILABLE),
    };
    // `resource_name()` is read BEFORE `verb` moves into `run_verb` below —
    // see `run_verb_within`'s own doc.
    let resource = verb.resource_name();
    rt.block_on(run_verb_within(
        resource,
        INVOCATION_TIMEOUT,
        run_verb(verb),
    ))
}

// ADR-038 §1 — the command policy table (167 rows) + its exactness test
// against `generate_handler!`. Data only in this phase: nothing here
// dispatches yet (§2's generic `agent call <ns>:<command>` tier is later).
pub(crate) mod policy;

// The MCP (Model Context Protocol) stdio server mode — `agent mcp`.
mod mcp;

#[cfg(test)]
mod tests;
