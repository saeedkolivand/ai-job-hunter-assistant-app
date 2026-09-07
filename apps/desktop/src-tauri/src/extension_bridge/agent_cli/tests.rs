use super::*;
// `advance_frame`/`ConnState`/`FrameDecision` are private to the parent
// `extension_bridge` module — visible here because privacy in Rust
// extends to every DESCENDANT module, not just direct children, so this
// test module (a grandchild) can reach them exactly as
// `extension_bridge::test` does one level up.
use super::super::{advance_frame, BridgeState, ConnState, FrameDecision};

// ── argv parsing ─────────────────────────────────────────────────────────

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn parses_best_matches_with_no_flags() {
    assert_eq!(
        parse_verb(&s(&["best-matches"])).unwrap(),
        Verb::BestMatches { limit: None }
    );
}

#[test]
fn parses_best_matches_with_limit() {
    assert_eq!(
        parse_verb(&s(&["best-matches", "--limit", "5"])).unwrap(),
        Verb::BestMatches { limit: Some(5) }
    );
}

#[test]
fn rejects_a_non_numeric_limit() {
    assert!(parse_verb(&s(&["best-matches", "--limit", "abc"])).is_err());
}

#[test]
fn rejects_limit_missing_its_value() {
    assert!(parse_verb(&s(&["best-matches", "--limit"])).is_err());
}

#[test]
fn parses_job_with_url() {
    assert_eq!(
        parse_verb(&s(&["job", "https://example.com/1"])).unwrap(),
        Verb::Job {
            url: "https://example.com/1".to_string()
        }
    );
}

#[test]
fn rejects_job_without_a_url() {
    assert!(parse_verb(&s(&["job"])).is_err());
}

#[test]
fn parses_found_jobs_with_just_an_autopilot_id() {
    assert_eq!(
        parse_verb(&s(&["found-jobs", "ap-1"])).unwrap(),
        Verb::FoundJobs {
            autopilot_id: "ap-1".to_string(),
            limit: None,
            cursor: None,
        }
    );
}

#[test]
fn parses_found_jobs_with_limit_and_cursor() {
    assert_eq!(
        parse_verb(&s(&[
            "found-jobs",
            "ap-1",
            "--limit",
            "50",
            "--cursor",
            "100"
        ]))
        .unwrap(),
        Verb::FoundJobs {
            autopilot_id: "ap-1".to_string(),
            limit: Some(50),
            cursor: Some("100".to_string()),
        }
    );
}

#[test]
fn rejects_found_jobs_without_an_autopilot_id() {
    assert!(parse_verb(&s(&["found-jobs"])).is_err());
}

#[test]
fn rejects_found_jobs_non_numeric_limit() {
    assert!(parse_verb(&s(&["found-jobs", "ap-1", "--limit", "abc"])).is_err());
}

#[test]
fn parses_the_three_no_arg_verbs() {
    assert_eq!(parse_verb(&s(&["profile"])).unwrap(), Verb::Profile);
    assert_eq!(parse_verb(&s(&["automations"])).unwrap(), Verb::Automations);
    assert_eq!(parse_verb(&s(&["schema"])).unwrap(), Verb::Schema);
}

#[test]
fn rejects_an_unknown_verb() {
    assert!(parse_verb(&s(&["delete-everything"])).is_err());
}

#[test]
fn rejects_a_missing_verb() {
    assert!(parse_verb(&s(&[])).is_err());
}

#[test]
fn unknown_verb_error_never_echoes_the_typed_argv_token() {
    // LOW fix (security review): argv can carry a path/username, and
    // this reply lands in an agent transcript — the error must list the
    // allowed verbs, never the token the caller typed.
    let leaky = r"C:\Users\alice\Desktop\secret-notes";
    let err = parse_verb(&s(&[leaky])).unwrap_err().to_string();
    assert!(
        !err.contains(leaky),
        "unknown-verb error must not echo argv: {err}"
    );
    assert!(
        err.contains("best-matches"),
        "must list the allowed verbs: {err}"
    );
}

#[test]
fn unknown_best_matches_flag_error_never_echoes_the_typed_argv_token() {
    // MINOR fix (security review round 2): the SAME leak one branch over
    // — `ajh-tauri agent best-matches "/home/alice/secret"` used to put
    // that path straight into the exit-2 `detail` field. Named flags
    // instead, mirroring the unknown-verb branch's own fix above.
    let leaky = r"C:\Users\alice\Desktop\secret-notes";
    let err = parse_verb(&s(&["best-matches", leaky]))
        .unwrap_err()
        .to_string();
    assert!(
        !err.contains(leaky),
        "unknown-argument error must not echo argv: {err}"
    );
    assert!(
        err.contains("--limit"),
        "must name the flag this verb accepts: {err}"
    );
}

// ── `call` argv parsing (ADR-038 §2, Phase 2) ───────────────────────────

#[test]
fn parses_call_with_namespace_command_and_input() {
    assert_eq!(
        parse_verb(&s(&["call", "jobs:jobs_list", "--input", r#"{"a":1}"#])).unwrap(),
        Verb::Call {
            namespace: "jobs".to_string(),
            command: "jobs_list".to_string(),
            input: serde_json::json!({ "a": 1 }),
            confirm: None,
        }
    );
}

#[test]
fn parses_call_without_input_as_an_empty_object() {
    assert_eq!(
        parse_verb(&s(&["call", "jobs:jobs_list"])).unwrap(),
        Verb::Call {
            namespace: "jobs".to_string(),
            command: "jobs_list".to_string(),
            input: serde_json::json!({}),
            confirm: None,
        }
    );
}

#[test]
fn parses_call_with_confirm() {
    assert_eq!(
        parse_verb(&s(&[
            "call",
            "documents:documents_remove",
            "--confirm",
            "Resume A"
        ]))
        .unwrap(),
        Verb::Call {
            namespace: "documents".to_string(),
            command: "documents_remove".to_string(),
            input: serde_json::json!({}),
            confirm: Some("Resume A".to_string()),
        }
    );
}

#[test]
fn parses_call_with_both_input_and_confirm_in_either_order() {
    let forward = parse_verb(&s(&[
        "call",
        "documents:documents_remove",
        "--input",
        r#"{"id":"doc-1"}"#,
        "--confirm",
        "Resume A",
    ]))
    .unwrap();
    let backward = parse_verb(&s(&[
        "call",
        "documents:documents_remove",
        "--confirm",
        "Resume A",
        "--input",
        r#"{"id":"doc-1"}"#,
    ]))
    .unwrap();
    assert_eq!(forward, backward);
    assert_eq!(
        forward,
        Verb::Call {
            namespace: "documents".to_string(),
            command: "documents_remove".to_string(),
            input: serde_json::json!({ "id": "doc-1" }),
            confirm: Some("Resume A".to_string()),
        }
    );
}

#[test]
fn rejects_confirm_missing_its_value() {
    assert!(parse_verb(&s(&["call", "jobs:jobs_list", "--confirm"])).is_err());
}

#[test]
fn confirm_error_never_echoes_the_typed_value() {
    // Same path-privacy discipline as `--input` — `--confirm` is user data
    // (ADR-038 §4) and must never appear in a usage error either.
    let leaky = r"C:\Users\alice\Desktop\secret-notes";
    let err = parse_verb(&s(&[
        "call",
        "jobs:jobs_list",
        "--confirm",
        leaky,
        "--bogus",
    ]))
    .unwrap_err()
    .to_string();
    assert!(!err.contains(leaky), "must not echo --confirm: {err}");
}

#[test]
fn rejects_call_missing_the_namespace_command_token() {
    assert!(parse_verb(&s(&["call"])).is_err());
}

#[test]
fn rejects_call_target_missing_a_colon() {
    assert!(parse_verb(&s(&["call", "jobs_list"])).is_err());
}

#[test]
fn rejects_call_target_with_an_empty_namespace_or_command() {
    assert!(parse_verb(&s(&["call", ":jobs_list"])).is_err());
    assert!(parse_verb(&s(&["call", "jobs:"])).is_err());
}

#[test]
fn rejects_call_input_that_is_not_valid_json() {
    let err = parse_verb(&s(&["call", "jobs:jobs_list", "--input", "{not json"]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("valid JSON"));
}

#[test]
fn rejects_call_input_that_is_not_a_json_object() {
    assert!(parse_verb(&s(&["call", "jobs:jobs_list", "--input", "[1,2]"])).is_err());
    assert!(parse_verb(&s(&["call", "jobs:jobs_list", "--input", "\"x\""])).is_err());
}

#[test]
fn call_input_error_never_echoes_the_typed_value() {
    // Path privacy — `--input` may carry a path or other sensitive content.
    let leaky = r#"{"path":"C:\Users\alice\Desktop\secret"NOTJSON"#;
    let err = parse_verb(&s(&["call", "jobs:jobs_list", "--input", leaky]))
        .unwrap_err()
        .to_string();
    assert!(!err.contains("alice"), "must not echo --input: {err}");
}

#[test]
fn call_verb_sends_the_agent_call_wire_type_and_expects_its_own_reply_type() {
    let verb = Verb::Call {
        namespace: "jobs".to_string(),
        command: "jobs_list".to_string(),
        input: serde_json::json!({}),
        confirm: None,
    };
    assert_eq!(verb.wire_type(), msg::AGENT_CALL);
    assert_eq!(verb.reply_type(), msg::AGENT_CALL_RESULT);
    assert_eq!(verb.resource_name(), "call");
    let payload = verb.payload();
    assert_eq!(payload["namespace"], "jobs");
    assert_eq!(payload["command"], "jobs_list");
    assert_eq!(payload["input"], serde_json::json!({}));
    assert!(
        payload.get("confirm").is_none(),
        "confirm must be absent from the payload when not supplied, not null"
    );
}

#[test]
fn call_verb_payload_carries_confirm_only_when_supplied() {
    let verb = Verb::Call {
        namespace: "documents".to_string(),
        command: "documents_remove".to_string(),
        input: serde_json::json!({ "id": "doc-1" }),
        confirm: Some("Resume A".to_string()),
    };
    assert_eq!(verb.payload()["confirm"], "Resume A");
}

#[test]
fn curated_verbs_still_send_agent_query_and_expect_agent_result() {
    assert_eq!(Verb::Schema.wire_type(), msg::AGENT_QUERY);
    assert_eq!(Verb::Schema.reply_type(), msg::AGENT_RESULT);
}

#[test]
fn exit_code_for_reply_reads_dispatched_for_call_and_ok_for_every_other_verb() {
    let call = Verb::Call {
        namespace: "jobs".to_string(),
        command: "jobs_list".to_string(),
        input: serde_json::json!({}),
        confirm: None,
    };
    assert_eq!(
        exit_code_for_reply(&call, &serde_json::json!({ "dispatched": true })),
        0
    );
    assert_eq!(
        exit_code_for_reply(&call, &serde_json::json!({ "dispatched": false })),
        2,
        "a call refusal is exit 2, never exit 1"
    );
    assert_eq!(
        exit_code_for_reply(&Verb::Schema, &serde_json::json!({ "ok": true })),
        0
    );
    assert_eq!(
        exit_code_for_reply(&Verb::Schema, &serde_json::json!({ "ok": false })),
        1
    );
}

/// ADR-038 §4 (Phase 3): "needs confirmation" is its OWN exit code, never
/// collapsed into the exit-2 "refusal" bucket every other `dispatched:false`
/// cause shares.
#[test]
fn exit_code_for_reply_reports_4_for_confirmation_required_and_2_for_every_other_refusal() {
    let call = Verb::Call {
        namespace: "documents".to_string(),
        command: "documents_remove".to_string(),
        input: serde_json::json!({}),
        confirm: None,
    };
    assert_eq!(
        exit_code_for_reply(
            &call,
            &serde_json::json!({ "dispatched": false, "error": "confirmation_required" }),
        ),
        4
    );
    for other_error in [
        "confirmation_mismatch",
        "proof_unavailable",
        "unknown_command",
    ] {
        assert_eq!(
            exit_code_for_reply(
                &call,
                &serde_json::json!({ "dispatched": false, "error": other_error }),
            ),
            2,
            "{other_error} must stay exit 2, not be confused with confirmation_required"
        );
    }
}

// ── --help / VERB_TABLE anti-drift (owner request) ──────────────────────
// Hand-written literal list, not derived from VERB_TABLE itself (mirrors
// the repo's standing "pair a loop-over-own-fields test with a
// hand-written literal list" lesson).

#[test]
fn verb_table_names_match_a_hand_written_literal_list() {
    let mut names: Vec<&str> = VERB_TABLE.iter().map(|v| v.name).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "automations",
            "best-matches",
            "call",
            "found-jobs",
            "job",
            "profile",
            "schema"
        ]
    );
}

/// Every verb [`VERB_TABLE`] names must actually be parseable — the
/// first half of the owner's anti-drift requirement.
#[test]
fn every_verb_in_the_table_is_parseable_with_its_minimal_args() {
    for v in VERB_TABLE {
        let args: Vec<String> = match v.name {
            "job" => s(&["job", "https://example.com/1"]),
            "found-jobs" => s(&["found-jobs", "ap-1"]),
            "call" => s(&["call", "jobs:jobs_list"]),
            other => s(&[other]),
        };
        assert!(
            parse_verb(&args).is_ok(),
            "verb `{}` listed in VERB_TABLE must parse",
            v.name
        );
    }
}

/// Every parseable verb must appear in the help text — the SECOND half
/// of the owner's anti-drift requirement (together with the test above,
/// this pins BOTH directions: help ⊆ parseable AND parseable ⊆ help).
#[test]
fn help_text_names_every_verb_in_the_table() {
    let text = help_text();
    for v in VERB_TABLE {
        assert!(
            text.contains(v.name),
            "help text is missing verb `{}`: {text}",
            v.name
        );
    }
    assert!(text.contains("--help"));
    assert!(text.contains("EXIT CODES"));
}

#[test]
fn help_text_lists_every_error_sentinel_this_cli_can_emit() {
    let text = help_text();
    for (sentinel, _) in ERROR_SENTINELS {
        assert!(
            text.contains(sentinel),
            "help text is missing sentinel `{sentinel}`: {text}"
        );
    }
}

#[test]
fn is_help_request_recognizes_help_h_and_bare_help_verb() {
    assert!(is_help_request(&s(&["--help"])));
    assert!(is_help_request(&s(&["-h"])));
    assert!(is_help_request(&s(&["help"])));
    assert!(is_help_request(&s(&["--help", "job"])));
    assert!(!is_help_request(&s(&["job", "--help"])));
    assert!(!is_help_request(&s(&["best-matches"])));
    assert!(!is_help_request(&s(&[])));
}

// ── mcp mode dispatch (owner request — `mcp` is a MODE, never a VERB_TABLE
// row, so the drift-loop tests above cannot see it) ─────────────────────

#[test]
fn help_text_mentions_the_mcp_mode() {
    assert!(help_text().contains("mcp"));
}

#[test]
fn help_text_indents_the_mcp_row_like_every_verb_row() {
    // LOW fix, review round 3 — the `\` line-continuation used to build this row strips ALL
    // leading whitespace off the continued line, so "mcp [...]" rendered flush-left instead of
    // indented two spaces like every VERB_TABLE row.
    let text = help_text();
    let mcp_line = text
        .lines()
        .find(|l| l.trim_start().starts_with("mcp "))
        .expect("must have an mcp row");
    assert!(
        mcp_line.starts_with("  mcp "),
        "the mcp row must be indented like every verb row: {mcp_line:?}"
    );
}

#[test]
fn is_mcp_mode_matches_only_the_exact_first_token() {
    assert!(is_mcp_mode(&s(&["mcp"])));
    assert!(is_mcp_mode(&s(&["mcp", "--allow-irreversible"])));
    assert!(!is_mcp_mode(&s(&["mcpx"])));
    assert!(!is_mcp_mode(&s(&["MCP"])));
    assert!(!is_mcp_mode(&s(&["--help", "mcp"])));
    assert!(!is_mcp_mode(&s(&[])));
}

#[test]
fn help_wins_over_mcp_when_help_is_the_first_token() {
    // `agent --help mcp` must be a help request, never mcp mode — `run()`
    // checks `is_help_request` FIRST, so the first-token-only check on
    // `is_mcp_mode` is what makes that ordering actually matter here.
    assert!(is_help_request(&s(&["--help", "mcp"])));
    assert!(!is_mcp_mode(&s(&["--help", "mcp"])));
}

// ── exit-2 shape uniformity (MINOR fix — security review round 2) ──────

#[test]
fn usage_error_value_carries_a_null_resource_key_like_every_other_exit_2_reply() {
    // Before the fix this branch's JSON had no `resource` key at all
    // (`{"detail":…,"error":"usage","ok":false}`) while `emit_cli_error`
    // always emits one — a consumer reading `resource` unconditionally
    // on every exit-2 reply got a missing key specifically here.
    let v = usage_error_value("unknown verb (run `ajh-tauri agent --help`)");
    assert!(
        v.get("resource").is_some(),
        "usage-error JSON must carry a `resource` key (null is fine): {v}"
    );
    assert!(v["resource"].is_null());
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"], ERR_USAGE);
}

#[test]
fn payload_carries_the_wire_resource_name() {
    assert_eq!(Verb::Schema.payload()["resource"], "schema");
    assert_eq!(
        Verb::Job {
            url: "https://x.example.com".to_string()
        }
        .payload()["url"],
        "https://x.example.com"
    );
    let with_limit = Verb::BestMatches { limit: Some(7) }.payload();
    assert_eq!(with_limit["limit"], 7);
    let without_limit = Verb::BestMatches { limit: None }.payload();
    assert!(without_limit.get("limit").is_none());
}

// ── pairing-failure classification (pure) ───────────────────────────────
// Hand-written expected buckets, not derived from `classify_pairing_failure`
// itself — mirrors the repo's standing lesson to pair a loop/derived check
// with a literal.

#[test]
fn all_ports_absent_is_app_not_running() {
    assert_eq!(
        classify_pairing_failure(&[PortOutcome::NoUpgrade, PortOutcome::NoUpgrade]),
        PairingFailure::AppNotRunning
    );
    assert_eq!(classify_pairing_failure(&[]), PairingFailure::AppNotRunning);
}

#[test]
fn every_reachable_port_rejecting_the_proof_is_pairing_rejected() {
    assert_eq!(
        classify_pairing_failure(&[PortOutcome::NoUpgrade, PortOutcome::ProofRejected]),
        PairingFailure::PairingRejected
    );
}

#[test]
fn any_pre_auth_error_is_connection_error_not_pairing_rejected() {
    // Issue #1084 PR1's own decision: "a crash between challenge and auth
    // is not a pairing failure" — even alongside a genuine proof
    // rejection on another port, the mixed case must NOT be reported as
    // a wrong token.
    assert_eq!(
        classify_pairing_failure(&[PortOutcome::PreAuthError, PortOutcome::ProofRejected]),
        PairingFailure::ConnectionError
    );
    assert_eq!(
        classify_pairing_failure(&[PortOutcome::PreAuthError]),
        PairingFailure::ConnectionError
    );
}

// ── handshake wire-shape round trip against the REAL server state
// machine (`super::advance_frame`) — no socket, no AppHandle needed:
// `advance_hello`/`advance_auth` are pure functions of `&BridgeState`.
// This is the proof the client's frame-building/parsing is wire-compatible
// with the committed server half, not just internally self-consistent. ──

#[test]
fn handshake_round_trips_against_the_real_server_state_machine() {
    let dir = tempfile::TempDir::new().unwrap();
    let state = BridgeState::load(dir.path());
    let token = state.token();

    let client_nonce = handshake::new_nonce();
    let hello = build_hello(&client_nonce);

    let decision = advance_frame(&state, &ConnState::AwaitingHello, &hello);
    let FrameDecision::Challenge { reply, next } = decision else {
        panic!("expected Challenge, got {decision:?}");
    };
    let challenge_json: Value = serde_json::from_str(&reply).unwrap();
    let server_nonce = parse_challenge(&challenge_json).expect("well-formed challenge");

    let proof = handshake::client_proof(&token, &server_nonce, &client_nonce);
    let auth = build_auth(&proof);

    let decision = advance_frame(&state, &next, &auth);
    let FrameDecision::AuthOk(reply) = decision else {
        panic!("expected AuthOk, got {decision:?}");
    };
    let auth_ok_json: Value = serde_json::from_str(&reply).unwrap();
    let server_proof = parse_auth_ok(&auth_ok_json).expect("well-formed auth.ok");

    assert!(
        handshake::verify_server_proof(&token, &server_nonce, &client_nonce, &server_proof),
        "the client's own verification must accept the real server's serverProof"
    );
}

#[test]
fn handshake_round_trip_rejects_a_wrong_token() {
    let dir = tempfile::TempDir::new().unwrap();
    let state = BridgeState::load(dir.path());

    let client_nonce = handshake::new_nonce();
    let hello = build_hello(&client_nonce);
    let decision = advance_frame(&state, &ConnState::AwaitingHello, &hello);
    let FrameDecision::Challenge { reply, next } = decision else {
        panic!("expected Challenge, got {decision:?}");
    };
    let server_nonce =
        parse_challenge(&serde_json::from_str(&reply).unwrap()).expect("well-formed challenge");

    // A wrong token — the CLI's persisted copy is stale.
    let wrong_proof = handshake::client_proof("not-the-real-token", &server_nonce, &client_nonce);
    let auth = build_auth(&wrong_proof);
    let decision = advance_frame(&state, &next, &auth);
    assert!(
        matches!(decision, FrameDecision::Unauthorized),
        "expected Unauthorized, got {decision:?}"
    );
}

// ── the SAME round trip, but over a REAL loopback socket, driving the
// production `attempt_port` fn (not a reimplementation) against a
// minimal server that itself calls the real `advance_frame` state
// machine — the strongest available proof that the client's transport
// code (WS upgrade, frame send/receive) interoperates with the actual
// server, not just that the JSON shapes match in-process. ──

#[tokio::test]
async fn attempt_port_authenticates_over_a_real_socket_against_the_real_server() {
    use tokio::net::TcpListener;

    let dir = tempfile::TempDir::new().unwrap();
    let state = BridgeState::load(dir.path());
    let token = state.token();

    // Kernel-assigned ephemeral port (never collides with a real running
    // app or another test) — same hermetic pattern as
    // `import_tests::claim_busy_port`.
    let listener = TcpListener::bind(("127.0.0.1", 0u16)).await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        // No Origin check here (that gate is `handle_connection`'s own,
        // covered by `auth`'s tests) — everything past the WS upgrade is
        // the real per-frame `advance_frame` dispatch `handle_connection`
        // itself runs.
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let mut conn = ConnState::AwaitingHello;
        loop {
            let msg = ws.next().await.unwrap().unwrap();
            let text = match msg {
                tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
                other => panic!("expected a text frame, got {other:?}"),
            };
            match advance_frame(&state, &conn, &text) {
                FrameDecision::Challenge { reply, next } => {
                    conn = next;
                    ws.send(tokio_tungstenite::tungstenite::Message::text(reply))
                        .await
                        .unwrap();
                }
                FrameDecision::AuthOk(reply) => {
                    ws.send(tokio_tungstenite::tungstenite::Message::text(reply))
                        .await
                        .unwrap();
                    break;
                }
                other => panic!("unexpected FrameDecision in test server: {other:?}"),
            }
        }
    });

    let result = attempt_port(port, &token).await;
    assert!(
        result.is_ok(),
        "attempt_port must authenticate against the real server over a real socket, got {:?}",
        result.err()
    );
    server.await.unwrap();
}

// ── `attempt_port`'s `connect`/WS-upgrade steps must be bounded (MAJOR
// fix — security review round 2): a local process that accepts a TCP
// connection on a candidate port and never completes the HTTP upgrade —
// including a wedged previous app instance whose listener is still
// bound but whose accept loop stopped running — used to park this fn,
// and so the whole invocation, forever. Drives the real production
// `attempt_port`, the same pattern as the real-socket test above, but
// against a server that accepts and then goes silent instead of ever
// speaking WebSocket. ──

#[tokio::test]
async fn attempt_port_gives_up_on_a_peer_that_accepts_and_never_completes_the_upgrade() {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0u16)).await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Accept the TCP connection (the kernel-level handshake a squatter
    // or a wedged app's still-bound listener completes for free) and
    // then go silent for the rest of this test: never send an HTTP
    // upgrade response, never close. This is exactly the "accepts on a
    // PORT_RANGE port and never completes the upgrade" scenario the fix
    // closes — before it, `client_async_with_config`'s read had no
    // deadline at all.
    let _server = tokio::spawn(async move {
        let (_tcp, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await
    });

    // Bounded well past 2× HANDSHAKE_STEP_TIMEOUT (connect can't
    // meaningfully stall against a listener that DID accept, so the
    // upgrade step's own timeout is what must fire here) so a
    // regression that restores the unbounded `.await` hangs this test
    // instead of the whole suite.
    let outcome = tokio::time::timeout(
        HANDSHAKE_STEP_TIMEOUT * 3,
        attempt_port(port, "irrelevant-token"),
    )
    .await;
    // `WsStream` doesn't implement `Debug`/`PartialEq` (it wraps a live
    // socket), so match the shape rather than `assert_eq!` the whole
    // `Result` or interpolate it into a panic message.
    match outcome {
        Ok(Err(PortOutcome::NoUpgrade)) => {}
        Ok(Ok(_)) => {
            panic!("attempt_port must not authenticate against a peer that never upgraded")
        }
        Ok(Err(other)) => panic!(
            "expected PortOutcome::NoUpgrade for a peer that never completes the upgrade, \
             got {other:?}"
        ),
        Err(_) => panic!(
            "attempt_port must give up once the upgrade step's own deadline elapses, not \
             hang forever on a peer that accepted but never completes the WS upgrade"
        ),
    }
}

// ── `next_json`'s deadline must cover the WHOLE call, not be re-armed
// per iteration — a peer that floods control frames (ping/pong) faster
// than the budget must not stall it past that budget. Drives the real
// production `next_json` over a real loopback socket against a minimal
// server, the same pattern as `attempt_port_authenticates_over_a_real_
// socket_against_the_real_server` above. ──

#[tokio::test]
async fn next_json_returns_at_its_deadline_even_when_flooded_with_pings() {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0u16)).await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Server: upgrade, then flood Ping frames faster than the client's
    // own per-call budget below — for as long as this task keeps
    // running (it is dropped, not joined, at the end of this test), so
    // a regression (a timeout re-armed on every iteration) would hang
    // past the outer bound below instead of merely returning late.
    let _server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        loop {
            if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let uri = format!("ws://127.0.0.1:{port}/").parse().unwrap();
    let (mut ws, _resp) = tokio_tungstenite::client_async(ClientRequestBuilder::new(uri), tcp)
        .await
        .unwrap();

    // The server's 10ms ping cadence is far faster than this budget, so
    // a correctly-fixed `next_json` still returns `None` right at the
    // deadline; a per-iteration-re-armed `timeout` (the bug) never
    // would, since every ping resets its clock — bound the assertion in
    // an outer timeout well past the budget so a regression fails this
    // test instead of hanging the whole suite.
    let budget = Duration::from_millis(150);
    let outcome = tokio::time::timeout(budget * 4, next_json(&mut ws, budget)).await;
    assert_eq!(
        outcome.ok(),
        Some(None),
        "next_json must return None at its own deadline, not hang past it, when flooded \
         with pings faster than that deadline"
    );
}

// ── `send_agent_query_within` (finding #7 fix — security review):
// distinguish a genuine timeout from an early transport failure, and
// fail fast on a same-`reqId` reply of the wrong type instead of waiting
// out the whole budget. All three drive the REAL production fn over a
// real loopback socket, the same pattern as `attempt_port_authenticates_
// over_a_real_socket_against_the_real_server`. ──

async fn connect_plain(port: u16) -> WsStream {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let uri = format!("ws://127.0.0.1:{port}/").parse().unwrap();
    tokio_tungstenite::client_async(ClientRequestBuilder::new(uri), tcp)
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn send_agent_query_within_reports_a_genuine_timeout_when_nothing_ever_replies() {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0u16)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        // Read and discard the query, then go silent for the rest of
        // this test — never replies, never closes.
        let _ = ws.next().await;
        std::future::pending::<()>().await
    });

    let ws = connect_plain(port).await;
    let budget = Duration::from_millis(150);
    // Bounded well past `budget` so a regression that hangs past the
    // deadline fails this test instead of the whole suite.
    let outcome = tokio::time::timeout(
        budget * 4,
        send_agent_query_within(ws, &Verb::Schema, budget),
    )
    .await;
    assert_eq!(
        outcome.ok(),
        Some(Err(ERR_TIMEOUT)),
        "a call that genuinely exhausts its budget must report `timeout`, not `connection_lost`"
    );
}

#[tokio::test]
async fn send_agent_query_within_reports_connection_lost_fast_on_an_early_close() {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0u16)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = ws.next().await; // read the query
                                 // Close immediately — well before any realistic budget.
    });

    let ws = connect_plain(port).await;
    // A generous budget — the point is proving this returns FAST, not
    // by waiting it out.
    let generous_budget = Duration::from_secs(5);
    let outcome = tokio::time::timeout(
        Duration::from_millis(500),
        send_agent_query_within(ws, &Verb::Schema, generous_budget),
    )
    .await;
    assert_eq!(
        outcome.ok(),
        Some(Err(ERR_CONNECTION_LOST)),
        "a close well before the deadline must never be reported as `timeout`"
    );
}

#[tokio::test]
async fn send_agent_query_within_fails_fast_on_a_same_req_id_wrong_type_reply() {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0u16)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let text = match msg {
            Message::Text(t) => t.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        };
        let sent: Value = serde_json::from_str(&text).unwrap();
        let req_id = sent["reqId"].as_str().unwrap().to_string();
        // Mirrors `advance_authenticated`'s "unknown message type"
        // fallback — a real (old) app that doesn't understand
        // `agent.query` replies exactly this way, echoing OUR reqId on
        // an `import.result` envelope.
        let reply = json!({
            "type": "import.result",
            "reqId": req_id,
            "payload": { "error": "unknown message type 'agent.query'" },
        })
        .to_string();
        let _ = ws.send(Message::text(reply)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    let ws = connect_plain(port).await;
    let generous_budget = Duration::from_secs(5);
    let outcome = tokio::time::timeout(
        Duration::from_millis(500),
        send_agent_query_within(ws, &Verb::Schema, generous_budget),
    )
    .await;
    assert_eq!(
        outcome.ok(),
        Some(Err(ERR_UNSUPPORTED_BY_APP)),
        "a same-reqId reply of the wrong type must fail fast as `unsupported_by_app`, \
         not silently `continue` toward a 30s timeout"
    );
}

// ── `run_verb_within` (MAJOR fix — security review round 2): the
// WHOLE-INVOCATION deadline, generic over both the budget and the inner
// future so it's testable without waiting out the real
// `INVOCATION_TIMEOUT` or standing up a pointer file/token/socket. ────────

#[tokio::test]
async fn run_verb_within_reports_timeout_when_the_inner_future_never_resolves() {
    let budget = Duration::from_millis(50);
    // Bounded well past `budget` so a regression that re-arms or drops
    // the deadline fails this test instead of hanging the suite.
    let outcome = tokio::time::timeout(
        budget * 4,
        run_verb_within("schema", budget, std::future::pending::<i32>()),
    )
    .await;
    assert_eq!(
        outcome.ok(),
        Some(2),
        "an expired overall deadline must exit 2, the same as any other exit-2 reply"
    );
}

#[tokio::test]
async fn run_verb_within_returns_the_inner_futures_own_exit_code_when_it_finishes_first() {
    // The normal case, unaffected by this fix: a `run_verb` that
    // finishes well inside its budget must return ITS OWN exit code
    // unchanged, never be reinterpreted by the deadline wrapper.
    let budget = Duration::from_secs(5);
    let code = run_verb_within("schema", budget, std::future::ready(0)).await;
    assert_eq!(code, 0);

    let code = run_verb_within("schema", budget, std::future::ready(1)).await;
    assert_eq!(code, 1);
}

// ── pointer + token file reads (pure fs, no env mutation needed here —
// the path itself is exercised by platform::config's own tests) ────────

#[test]
fn read_pairing_token_trims_and_rejects_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join(TOKEN_FILE), "  abc123  \n").unwrap();
    assert_eq!(
        read_pairing_token(dir.path().to_str().unwrap()),
        Some("abc123".to_string())
    );

    let empty_dir = tempfile::TempDir::new().unwrap();
    std::fs::write(empty_dir.path().join(TOKEN_FILE), "   \n").unwrap();
    assert_eq!(read_pairing_token(empty_dir.path().to_str().unwrap()), None);

    let missing_dir = tempfile::TempDir::new().unwrap();
    assert_eq!(
        read_pairing_token(missing_dir.path().to_str().unwrap()),
        None
    );
}

// ── UNC `dataDir` guard (MEDIUM fix — security review) ─────────────────

#[test]
fn rejects_unc_and_double_slash_data_dirs() {
    assert!(!is_safe_local_data_dir(r"\\attacker.example.com\share"));
    assert!(!is_safe_local_data_dir("//attacker.example.com/share"));
    // A mixed-separator UNC path is still UNC.
    assert!(!is_safe_local_data_dir(r"\\attacker.example.com/share"));
}

/// MEDIUM fix, security review round 2: Windows treats `/` and `\`
/// interchangeably, so a UNC root written with ONE separator of each
/// kind (`\/host\share`, `/\host/share`) is still an absolute UNC path —
/// confirmed against `ntpath`'s parser — and the pre-fix
/// `starts_with(r"\\") || starts_with("//")` check let both straight
/// through, since neither is a literal two-backslash or two-slash
/// prefix.
#[test]
fn rejects_a_unc_data_dir_with_mixed_leading_separators() {
    assert!(!is_safe_local_data_dir(r"\/attacker.example.com\share"));
    assert!(!is_safe_local_data_dir(r"/\attacker.example.com/share"));
}

#[test]
fn rejects_a_relative_data_dir() {
    assert!(!is_safe_local_data_dir("relative/path"));
    assert!(!is_safe_local_data_dir(""));
}

#[test]
fn accepts_a_normal_absolute_local_path() {
    let dir = tempfile::TempDir::new().unwrap();
    assert!(is_safe_local_data_dir(dir.path().to_str().unwrap()));
}

#[test]
fn read_pairing_token_never_touches_the_filesystem_for_a_unc_data_dir() {
    // No real UNC share exists in a hermetic test — the proof this guard
    // works is that it returns `None` WITHOUT ever attempting the read
    // (a real attempt against an unreachable host would hang/timeout,
    // not return promptly).
    assert_eq!(read_pairing_token(r"\\attacker.example.com\share"), None);
}
