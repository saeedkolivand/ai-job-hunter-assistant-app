use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::*;

/// How long a test waits for a signal that a correct [`serve`] always sends — long enough that a
/// loaded CI machine never trips it, short enough that a real deadlock fails the run rather than
/// hanging it.
const SIGNAL_BUDGET: Duration = Duration::from_secs(10);

fn line(v: Value) -> String {
    format!("{v}\n")
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// The two halves of one `tools/call`, composed. Production never needs this — [`serve`] runs
/// [`classify_tool_call`] on its writer thread and [`dispatched_tool_result`] on the worker,
/// which is the whole point of the split — so the composition lives here rather than as a
/// never-called fn in `mcp.rs`. The real loop's own composition is covered by the `serve` tests
/// below, not by this helper.
fn tool_call_result(
    params: &Value,
    server: &Server,
    dispatch: &mut dyn FnMut(&Verb) -> Result<Value, &'static str>,
) -> Result<Value, (i64, &'static str)> {
    match classify_tool_call(params, server) {
        ToolCall::Local(outcome) => outcome,
        ToolCall::Bridge(verb) => Ok(dispatched_tool_result(&verb, dispatch)),
    }
}

fn stub_ok(_verb: &Verb) -> Result<Value, &'static str> {
    Ok(json!({ "ok": true, "resource": "stub", "data": {} }))
}

/// A poisoned `Mutex` in a test is a panic in ANOTHER test thread; surface it as a failure here
/// rather than propagating an `unwrap` chain through every assertion.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Drive [`serve`] over an in-memory [`Cursor`] with a stub dispatcher — no runtime, no socket, no
/// live app. Always the most permissive launch mode (both flags) unless a test needs otherwise.
/// The dispatch stub now runs on `serve`'s own worker thread, so it must be `Send + 'static`:
/// state a test wants to observe travels through an `Arc` (or a channel), never a borrow.
fn serve_with(
    input: &str,
    output: impl Write,
    dispatch: impl FnMut(&Verb) -> Result<Value, &'static str> + Send + 'static,
) -> i32 {
    serve_with_drain_budget(input, output, dispatch, INVOCATION_TIMEOUT)
}

/// [`serve_with`] with the EOF drain deadline injected — production's own
/// [`INVOCATION_TIMEOUT`] everywhere except the two tests that measure the deadline itself, which
/// would otherwise have to wait it out.
fn serve_with_drain_budget(
    input: &str,
    output: impl Write,
    dispatch: impl FnMut(&Verb) -> Result<Value, &'static str> + Send + 'static,
    drain_budget: Duration,
) -> i32 {
    let server = Server::new(true, true);
    serve(
        Cursor::new(input.to_string()),
        output,
        &server,
        dispatch,
        drain_budget,
    )
}

/// [`serve_with`] into a plain buffer — returns the raw stdout bytes as a `String` so a test can
/// assert exact line counts / content.
fn run_serve(
    input: &str,
    dispatch: impl FnMut(&Verb) -> Result<Value, &'static str> + Send + 'static,
) -> String {
    let mut output = Vec::new();
    let code = serve_with(input, &mut output, dispatch);
    assert_eq!(code, 0);
    String::from_utf8(output).expect("valid utf8")
}

/// Every `id` [`serve`] wrote, in write order.
fn reply_ids(text: &str) -> Vec<i64> {
    text.lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).expect("each line is one JSON-RPC reply")["id"]
                .as_i64()
                .expect("every reply here carries a numeric id")
        })
        .collect()
}

/// [`serve`]'s single writer, instrumented: mirrors every byte into a shared buffer and, the
/// first time that buffer contains `needle`, pulses `signal` exactly once. Lets a dispatch stub
/// running on the WORKER thread block until a frame the MAIN thread emitted has really been
/// written — the only way to observe "answered mid-call" from outside.
struct SignallingWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
    needle: &'static str,
    signal: Option<std::sync::mpsc::Sender<()>>,
}

impl Write for SignallingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let seen = {
            let mut sink = lock(&self.buffer);
            sink.extend_from_slice(buf);
            String::from_utf8_lossy(&sink).contains(self.needle)
        };
        if seen {
            if let Some(tx) = self.signal.take() {
                let _ = tx.send(());
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A writer whose every write fails — the EPIPE a client that closed its pipe produces.
struct BrokenWriter;

impl Write for BrokenWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("client closed the pipe"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("client closed the pipe"))
    }
}

/// A writer that PARKS inside its FIRST `write` — pulsing `parked` first — until the test drops
/// its release sender, then accepts everything. The client that stopped draining stdout, held
/// still on purpose: while it is parked the whole loop is stuck inside [`emit`], which is the
/// only state in which the reader thread can be observed running ahead of the writer.
struct ParkingWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
    parked: Option<std::sync::mpsc::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Write for ParkingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(tx) = self.parked.take() {
            let _ = tx.send(());
            // Returns as soon as the test drops the sender (Disconnected); the budget is only
            // there so a broken test fails instead of hanging the run.
            let _ = self.release.recv_timeout(SIGNAL_BUDGET);
        }
        lock(&self.buffer).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A [`BufRead`] that hands out ONE line per `fill_buf` and counts every line it has handed over.
/// A [`Cursor`] cannot answer the question the bound is about — "how far did the reader get before
/// it stopped?" — because it is consumed in whatever chunks the reader asks for; this counts the
/// lines the reader thread actually pulled, so a reader parked on a full queue and a reader that
/// swallowed the entire input are two different numbers.
struct PacedInput {
    lines: std::vec::IntoIter<String>,
    current: Vec<u8>,
    pos: usize,
    produced: Arc<AtomicUsize>,
}

impl std::io::Read for PacedInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let taken = {
            let available = self.fill_buf()?;
            let n = available.len().min(buf.len());
            buf[..n].copy_from_slice(&available[..n]);
            n
        };
        self.consume(taken);
        Ok(taken)
    }
}

impl std::io::BufRead for PacedInput {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.pos == self.current.len() {
            self.current = self.lines.next().unwrap_or_default().into_bytes();
            self.pos = 0;
            if !self.current.is_empty() {
                // Counted on HAND-OVER, so the count is "lines the reader has begun reading",
                // never "lines the test wrote".
                self.produced.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(&self.current[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.current.len());
    }
}

fn names(list: &[Value]) -> Vec<&str> {
    let mut n: Vec<&str> = list.iter().map(|t| t["name"].as_str().unwrap()).collect();
    n.sort_unstable();
    n
}

// ── serve — the pure loop over a Cursor (GRAFT: mutation-visible) ─────────

#[test]
fn serve_emits_exactly_one_line_per_request_and_none_for_notifications() {
    let input = format!(
        "{}{}{}",
        line(json!({"jsonrpc":"2.0","id":1,"method":"ping"})),
        line(json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
        line(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})),
    );
    let text = run_serve(&input, stub_ok);
    assert_eq!(
        text.lines().count(),
        2,
        "the notification must produce no output line: {text:?}"
    );
}

/// The `[call, ping, call]` input both concurrency tests below drive.
fn sandwiched_ping_input() -> String {
    format!(
        "{}{}{}",
        line(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "job", "arguments": { "url": "https://example.com/first" } },
        })),
        line(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" })),
        line(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "job", "arguments": { "url": "https://example.com/second" } },
        })),
    )
}

#[test]
fn a_ping_is_answered_while_the_first_tools_call_is_still_in_flight() {
    // ADR-040 §12's follow-up, pinned: the first call's dispatch BLOCKS on the worker thread
    // until the main thread has written the sandwiched ping's reply, so the emitted ids must be
    // [2, 1, 3] — id 2 answered mid-call, ahead of the earlier request it overtook. Under the old
    // read→handle→write loop this deadlocks (the ping can't be written until the call it queues
    // behind returns), which is exactly the property under test.
    let (ping_seen, wait_for_ping) = std::sync::mpsc::channel::<()>();
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let output = SignallingWriter {
        buffer: Arc::clone(&buffer),
        // The ping's own reply frame; no other frame in this input carries it.
        needle: "\"id\":2",
        signal: Some(ping_seen),
    };

    let dispatch_order = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&dispatch_order);
    let code = serve_with(&sandwiched_ping_input(), output, move |verb: &Verb| {
        if let Verb::Job { url } = verb {
            let first = {
                let mut seen = lock(&recorded);
                seen.push(url.clone());
                seen.len() == 1
            };
            if first {
                // Hold the worker inside the FIRST dispatch until the ping has been written.
                let _ = wait_for_ping.recv_timeout(SIGNAL_BUDGET);
            }
        }
        Ok(json!({ "ok": true, "resource": "job", "data": {} }))
    });
    assert_eq!(code, 0);

    let text = String::from_utf8(lock(&buffer).clone()).expect("valid utf8");
    assert_eq!(
        reply_ids(&text),
        vec![2, 1, 3],
        "the ping (id 2) must be answered while the first call (id 1) is still in flight, and \
         the second call (id 3) only after it: {text:?}"
    );
    assert_eq!(
        *lock(&dispatch_order),
        vec!["https://example.com/first", "https://example.com/second"],
        "the two tools/call frames must still dispatch in INPUT order — the sandwiched ping \
         never dispatches at all"
    );
}

#[test]
fn a_local_tools_call_is_answered_while_a_bridge_call_is_still_in_flight() {
    // The follow-up to the ping guarantee: `commands` is LOCAL (it reads this binary's own POLICY
    // copy and never opens a bridge connection), so it must not wait behind a bridge-backed call
    // either. The stub blocks the `job` dispatch until the `commands` reply has been written, so
    // the ids must come back [2, 1] — and the stub must be entered exactly ONCE, since a local
    // tool never reaches the worker at all.
    let (local_seen, wait_for_local) = std::sync::mpsc::channel::<()>();
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let output = SignallingWriter {
        buffer: Arc::clone(&buffer),
        // The `commands` reply's own frame; the blocked `job` call carries id 1.
        needle: "\"id\":2",
        signal: Some(local_seen),
    };
    let input = format!(
        "{}{}",
        line(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "job", "arguments": { "url": "https://example.com/slow" } },
        })),
        line(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "commands", "arguments": { "effect": "read" } },
        })),
    );

    let dispatches = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&dispatches);
    let code = serve_with(&input, output, move |_: &Verb| {
        counted.fetch_add(1, Ordering::SeqCst);
        let _ = wait_for_local.recv_timeout(SIGNAL_BUDGET);
        Ok(json!({ "ok": true, "resource": "job", "data": {} }))
    });
    assert_eq!(code, 0);

    let text = String::from_utf8(lock(&buffer).clone()).expect("valid utf8");
    assert_eq!(
        reply_ids(&text),
        vec![2, 1],
        "the local `commands` call (id 2) must be answered while the bridge call (id 1) is \
         still in flight: {text:?}"
    );
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        1,
        "only the bridge-backed `job` call may reach the dispatcher — `commands` is answered \
         without ever touching the wire"
    );
}

#[test]
fn a_locally_refused_tools_call_is_answered_without_reaching_the_dispatcher() {
    // The other local class: `local_call_refusal` (here a wrong_tool refusal for a real
    // Reversible row named on call-read). It is decided from the bundled POLICY copy, so it must
    // never be queued behind a dispatch either — and must never BE one.
    let input = line(json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {
            "name": "call-read",
            "arguments": { "namespace": "cli_agents", "command": "cli_agents_redetect" },
        },
    }));
    let dispatched = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dispatched);
    let text = run_serve(&input, move |_: &Verb| {
        flag.store(true, Ordering::SeqCst);
        Ok(json!({ "ok": true }))
    });

    let reply: Value = serde_json::from_str(text.trim()).expect("one reply frame");
    let payload: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap())
            .expect("content[0] is the refusal payload");
    assert_eq!(payload["error"], "wrong_tool");
    assert!(
        !dispatched.load(Ordering::SeqCst),
        "a local refusal must be answered without a bridge dispatch"
    );
}

#[test]
fn two_tools_calls_never_dispatch_concurrently() {
    // The other half of the guarantee: answering a ping mid-call must not have made DISPATCH
    // concurrent. The stub records entry/exit and the loop's peak occupancy must stay 1, so a
    // second bridge connection can never be open while the first is (the ADR-040 §12 throttle
    // bound). Mutation-visible: spawning per call instead of queueing onto one worker pushes the
    // peak to 2.
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let dispatched = Arc::new(AtomicUsize::new(0));
    let (entered, peaked, counted) = (
        Arc::clone(&in_flight),
        Arc::clone(&peak),
        Arc::clone(&dispatched),
    );

    let text = run_serve(&sandwiched_ping_input(), move |_: &Verb| {
        let now = entered.fetch_add(1, Ordering::SeqCst) + 1;
        peaked.fetch_max(now, Ordering::SeqCst);
        counted.fetch_add(1, Ordering::SeqCst);
        // A real dispatch is not instantaneous; give an overlapping one room to be observed.
        std::thread::sleep(Duration::from_millis(20));
        entered.fetch_sub(1, Ordering::SeqCst);
        Ok(json!({ "ok": true, "resource": "job", "data": {} }))
    });

    assert_eq!(
        dispatched.load(Ordering::SeqCst),
        2,
        "both tools/call frames must dispatch: {text:?}"
    );
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "at most ONE dispatch may ever be in flight — see the module doc's single-flight guarantee"
    );
    assert_eq!(
        in_flight.load(Ordering::SeqCst),
        0,
        "every dispatch must have completed before serve returned"
    );
}

#[test]
fn a_write_failure_ends_serve_with_exit_zero_and_dispatches_nothing_further() {
    // EPIPE once the client closes its end of the pipe: `emit`'s `Err` is the cue to STOP, never
    // a retry and never a panic (release is `panic = "abort"`, where a panic is a silent death).
    // The second assertion is what makes this mutation-visible: dropping the early return leaves
    // the exit code 0 either way, but the `tools/call` behind the failed ping would then still
    // reach the bridge, on a connection whose reply can no longer be delivered.
    let input = format!(
        "{}{}",
        line(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
        line(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "profile", "arguments": {} },
        })),
    );
    let dispatched = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dispatched);
    let code = serve_with(&input, BrokenWriter, move |_: &Verb| {
        flag.store(true, Ordering::SeqCst);
        Ok(json!({ "ok": true }))
    });
    assert_eq!(code, 0, "a dead pipe is a clean exit, never a non-zero one");
    assert!(
        !dispatched.load(Ordering::SeqCst),
        "the early return on a failed write must stop the loop from ROUTING the frames behind \
         it: the tools/call after the failed ping is never classified, so it never reaches the \
         dispatcher"
    );
}

// ── The bounded dispatch queue (item 6) ──────────────────────────────────

/// The queue between the writer thread and the single-flight dispatcher is BOUNDED, and a full
/// queue is answered rather than waited on: the excess `tools/call` comes back as a `server_busy`
/// tool result WHILE the first dispatch is still blocked. Mutation-visible twice over — an
/// unbounded `channel()` produces zero refusals, and a blocking `send` on a full one writes
/// nothing at all until the dispatch below is released, so the signal never pulses and this test
/// fails on the timeout instead of the assertion.
#[test]
fn a_full_dispatch_queue_is_refused_with_server_busy_while_a_call_is_in_flight() {
    let total = MCP_CALL_QUEUE_MAX + 4;
    let input: String = (1..=total)
        .map(|id| {
            line(json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "profile", "arguments": {} },
            }))
        })
        .collect();

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let (seen_busy, busy_written) = std::sync::mpsc::channel::<()>();
    let writer = SignallingWriter {
        buffer: Arc::clone(&buffer),
        needle: "server_busy",
        signal: Some(seen_busy),
    };
    // Every dispatch parks here until the test drops the sender, so the worker is provably still
    // holding the first call when the refusals are written.
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let dispatched = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&dispatched);

    let server = std::thread::spawn(move || {
        serve_with(&input, writer, move |_: &Verb| {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = blocked.recv_timeout(SIGNAL_BUDGET);
            Ok(json!({ "ok": true, "resource": "profile", "data": {} }))
        })
    });

    busy_written
        .recv_timeout(SIGNAL_BUDGET)
        .expect("a server_busy refusal must be written while the dispatcher is blocked");
    // Read BEFORE releasing: once the blocker is gone the worker drains the queue and frees
    // slots, so how the remaining frames split between accepted and refused stops being a
    // property of the bound and starts being a race.
    let running_at_refusal = dispatched.load(Ordering::SeqCst);
    drop(release);
    let code = server.join().expect("serve must not panic");
    assert_eq!(code, 0);

    let text = String::from_utf8(lock(&buffer).clone()).expect("valid utf8");
    let replies: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is one reply"))
        .collect();
    assert_eq!(
        replies.len(),
        total,
        "every frame must be answered exactly once, refused or dispatched: {text:?}"
    );
    let busy: Vec<&Value> = replies
        .iter()
        .filter(|r| {
            r["result"]["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("server_busy"))
        })
        .collect();
    assert!(
        !busy.is_empty(),
        "the queue is bounded at {MCP_CALL_QUEUE_MAX}, so {total} pipelined calls must produce \
         at least one refusal: {text:?}"
    );
    for refusal in &busy {
        assert_eq!(refusal["result"]["isError"], true);
        assert_eq!(refusal["result"]["content"][1]["text"], "exitCode: 2");
        let payload: Value =
            serde_json::from_str(refusal["result"]["content"][0]["text"].as_str().unwrap())
                .expect("content[0] is the refusal payload");
        assert_eq!(payload["error"], "server_busy");
        assert_eq!(payload["dispatched"], false);
    }
    assert_eq!(
        dispatched.load(Ordering::SeqCst),
        total - busy.len(),
        "a refused call must never also reach the bridge"
    );
    // `<= 1`, not `== 1`: at most one call may be RUNNING when the refusal is written, so the
    // rest were WAITING in a full queue rather than being dispatched. Zero is legitimate and was
    // a real flake — 2 of 6 local runs, on this assertion, before this branch changed anything:
    // the writer can fill a {MCP_CALL_QUEUE_MAX}-deep queue and refuse the next frame in
    // microseconds, and the dispatch thread is not guaranteed to have been SCHEDULED by then. A
    // full queue is precisely the state that does not require it to have run. The upper bound is
    // what carries the meaning and is the mutation-visible half: dispatch per call instead of
    // onto one worker and this reads well above 1 (if `busy_written` above even fires at all).
    assert!(
        running_at_refusal <= 1,
        "at most ONE call may have been running when the refusal was written, so the other \
         {MCP_CALL_QUEUE_MAX} were waiting in a full queue rather than being dispatched; \
         {running_at_refusal} were running"
    );
}

// ── The bounded reader → writer event queue ──────────────────────────────

/// The OTHER half of the backpressure the reader split lost. Bounding the DISPATCH queue only
/// stopped `tools/call` frames piling up; a client that stops draining stdout parks the loop
/// inside `emit`, and with an unbounded `Event` channel the reader would go on turning a
/// never-blocking stdin (a file, or a pipelining client) into `Event::Line`s without limit.
///
/// Measured as the reader's own progress, which is the only place the difference shows: with the
/// writer held still, the reader may hand over exactly [`MCP_EVENT_QUEUE_MAX`] queued lines, plus
/// the one the loop already took out of the queue, plus the one it is parked in `send` holding —
/// and then it must STOP. Mutation-visible and not by a hair: swap the `sync_channel` back for a
/// `channel()` and the reader swallows the whole input in the same microseconds, so the exact
/// count below is out by a factor of four rather than by one frame.
#[test]
fn a_parked_writer_stops_the_reader_at_the_event_queue_bound() {
    // Four times the bound, so "stopped at the bound" and "read the whole input" are nowhere
    // near each other.
    let total = MCP_EVENT_QUEUE_MAX * 4 + 16;
    let lines: Vec<String> = (1..=total)
        .map(|id| line(json!({ "jsonrpc": "2.0", "id": id, "method": "ping" })))
        .collect();
    let produced = Arc::new(AtomicUsize::new(0));
    let input = PacedInput {
        lines: lines.into_iter(),
        current: Vec::new(),
        pos: 0,
        produced: Arc::clone(&produced),
    };

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let (parked_tx, writer_parked) = std::sync::mpsc::channel::<()>();
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let writer = ParkingWriter {
        buffer: Arc::clone(&buffer),
        parked: Some(parked_tx),
        release: blocked,
    };

    let server = std::thread::spawn(move || {
        let server = Server::new(true, true);
        serve(input, writer, &server, stub_ok, INVOCATION_TIMEOUT)
    });

    writer_parked
        .recv_timeout(SIGNAL_BUDGET)
        .expect("the writer must reach its first frame");

    // `+ 2`: the line the loop pulled out of the queue before parking in `emit`, and the line the
    // reader is parked in `send` holding. Everything else must still be unread.
    let ceiling = MCP_EVENT_QUEUE_MAX + 2;
    let deadline = std::time::Instant::now() + SIGNAL_BUDGET;
    while produced.load(Ordering::SeqCst) < ceiling && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    // The reader is now against the wall (or the assertion below says it never got there). The
    // settle exists for the MUTATION direction only — it gives an unbounded reader, which needs
    // microseconds for the remaining lines, all the room it could want to run away in. A correct
    // reader cannot move at all while the writer is parked, so this changes nothing here.
    std::thread::sleep(Duration::from_millis(50));
    let while_parked = produced.load(Ordering::SeqCst);
    assert_eq!(
        while_parked, ceiling,
        "with the writer parked, the reader must stop at the {MCP_EVENT_QUEUE_MAX}-deep event \
         queue (+2 in flight) rather than buffering all {total} lines of a stdin that never \
         blocks on its own"
    );

    drop(release);
    let code = server.join().expect("serve must not panic");
    assert_eq!(code, 0);

    // Backpressure, not loss: once the writer drains, the reader resumes and every frame is still
    // answered exactly once — the bound would be worthless if it dropped lines to hold.
    let text = String::from_utf8(lock(&buffer).clone()).expect("valid utf8");
    assert_eq!(
        reply_ids(&text),
        (1..=total as i64).collect::<Vec<i64>>(),
        "every line must still be answered once, in order, after the writer unblocks"
    );
    assert_eq!(
        produced.load(Ordering::SeqCst),
        total,
        "the reader must have gone on to read the rest of the input, not abandoned it"
    );
}

// ── The bounded EOF drain (item 7) ───────────────────────────────────────

/// The drain budget, and the sleep a blocking dispatch holds the worker for. An ORDER OF
/// MAGNITUDE apart in each direction from the wall each measures (CodeRabbit, PR #1092 — at
/// 50 ms/300 ms the "returned on its own deadline" assertion below had only 250 ms of scheduling
/// slack, so a loaded CI runner could fail a correct build): `serve` must return on the 50 ms
/// deadline, the assertion allows it 20× that, and the dispatch it must NOT wait out runs 40×
/// it. Only `DRAIN_EXIT_MAX` sits between the two, and it is nowhere near either.
const DRAIN_BUDGET: Duration = Duration::from_millis(50);
const DISPATCH_HOLD: Duration = Duration::from_secs(2);
const DRAIN_EXIT_MAX: Duration = Duration::from_secs(1);

/// After `Eof` the drain has ONE absolute deadline, not one `INVOCATION_TIMEOUT` per queued call:
/// with a blocking dispatcher and a short injected budget, `serve` must return 0 long before the
/// in-flight call finishes, and the calls still queued behind it must never dispatch at all.
/// Mutation-visible: remove the deadline and this waits out the sleep below; keep the deadline
/// but drop the worker's abandoned-flag check and the queued second call still dispatches.
///
/// And every call the client is still waiting on is ANSWERED before the exit — the half of the
/// guarantee that used to be silence. Mutation-visible on its own: delete the `shutting_down`
/// sweep and this writes nothing at all, which is what it asserted before the fix.
#[test]
fn an_expired_drain_deadline_answers_what_it_abandons_and_dispatches_nothing_further() {
    let input: String = (1..=2)
        .map(|id| {
            line(json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "profile", "arguments": {} },
            }))
        })
        .collect();

    // The stub reports each entry and each exit, so "the second call never dispatched" is a
    // message that never arrives rather than a fixed sleep.
    let (report, progress) = std::sync::mpsc::channel::<&'static str>();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let mut output = Vec::new();

    let started = std::time::Instant::now();
    let code = serve_with_drain_budget(
        &input,
        &mut output,
        move |_: &Verb| {
            let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
            let _ = report.send(if first { "enter-1" } else { "enter-2" });
            std::thread::sleep(DISPATCH_HOLD);
            let _ = report.send(if first { "exit-1" } else { "exit-2" });
            Ok(json!({ "ok": true, "resource": "profile", "data": {} }))
        },
        DRAIN_BUDGET,
    );

    assert_eq!(code, 0, "an expired drain is still a clean exit");
    assert!(
        started.elapsed() < DRAIN_EXIT_MAX,
        "serve must return on its own deadline, not wait out the in-flight dispatch \
         (took {:?})",
        started.elapsed()
    );
    // Both calls are answered — the abandoned one and the never-started one — and the two
    // answers differ in the one fact the client needs to decide whether repeating is safe.
    let text = String::from_utf8(output).expect("valid utf8");
    let replies: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is one reply"))
        .collect();
    assert_eq!(
        reply_ids(&text),
        vec![1, 2],
        "every call the client was still waiting on must be answered, in queue order: {text:?}"
    );
    let payload = |r: &Value| -> Value {
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap())
            .expect("content[0] is the refusal payload")
    };
    for reply in &replies {
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["content"][1]["text"], "exitCode: 2");
        assert_eq!(payload(reply)["error"], "shutting_down");
    }
    assert_eq!(
        payload(&replies[0])["dispatched"],
        true,
        "the in-flight call reached the app and may have taken effect — saying otherwise would \
         invite a client to repeat a write that already landed"
    );
    assert_eq!(
        payload(&replies[1])["dispatched"],
        false,
        "the queued call provably never reached the app: {text:?}"
    );

    assert_eq!(
        progress.recv_timeout(SIGNAL_BUDGET).ok(),
        Some("enter-1"),
        "the first call must have started before the deadline expired"
    );
    assert_eq!(
        progress.recv_timeout(SIGNAL_BUDGET).ok(),
        Some("exit-1"),
        "the abandoned in-flight call still runs to completion on its own thread"
    );
    // The queued second call must never start. A violating build reaches it the INSTANT the
    // first returns — i.e. immediately after the `exit-1` just received — so this grace only has
    // to outlast a thread wake-up, and stays far below `DISPATCH_HOLD` so a correct build never
    // waits it out for nothing.
    assert!(
        progress.recv_timeout(DRAIN_EXIT_MAX).is_err(),
        "a call still queued when the drain deadline expired must never dispatch"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn eof_still_writes_the_reply_of_a_call_that_was_already_in_flight() {
    // The drain guarantee: stdin closes immediately after a single `tools/call` line, so the
    // `Eof` event reaches the loop while the worker is still inside the dispatch. The reply must
    // still be written before `serve` returns, never dropped on the floor.
    let input = line(json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "profile", "arguments": {} },
    }));
    let text = run_serve(&input, |_: &Verb| {
        std::thread::sleep(Duration::from_millis(100));
        Ok(json!({ "ok": true, "resource": "profile", "data": {} }))
    });
    assert_eq!(
        reply_ids(&text),
        vec![7],
        "the in-flight call's reply must survive EOF: {text:?}"
    );
}

#[test]
fn an_explicit_id_null_produces_no_output_and_no_dispatch() {
    let input = line(json!({
        "jsonrpc": "2.0", "id": null, "method": "tools/call",
        "params": { "name": "profile", "arguments": {} },
    }));
    let dispatched = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dispatched);
    let text = run_serve(&input, move |_: &Verb| {
        flag.store(true, Ordering::SeqCst);
        Ok(json!({ "ok": true }))
    });
    assert!(
        text.is_empty(),
        "id:null must produce zero output: {text:?}"
    );
    assert!(
        !dispatched.load(Ordering::SeqCst),
        "id:null must never reach the worker — nothing is listening for the result"
    );
}

#[test]
fn a_missing_id_member_is_treated_as_a_notification() {
    let input = line(json!({"jsonrpc":"2.0","method":"ping"}));
    let text = run_serve(&input, stub_ok);
    assert!(text.is_empty());
}

#[test]
fn server_discover_is_plain_method_not_found() {
    let input = line(json!({"jsonrpc":"2.0","id":1,"method":"server/discover"}));
    let text = run_serve(&input, stub_ok);
    let reply: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(reply["error"]["code"], -32601);
}

#[test]
fn unparseable_json_is_a_parse_error() {
    let text = run_serve("not json at all\n", stub_ok);
    let reply: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(reply["error"]["code"], -32700);
}

#[test]
fn a_fenced_payload_containing_newlines_still_emits_exactly_one_line() {
    let fenced = crate::prompt_fence::fenced(
        "job_posting",
        "line one\nline two\nthree",
        crate::prompt_fence::JOB_CAP,
    );
    let input = line(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "job", "arguments": { "url": "https://example.com/1" } },
    }));
    let text = run_serve(&input, move |_: &Verb| {
        Ok(json!({ "ok": true, "resource": "job", "data": { "description": fenced.clone() } }))
    });
    assert_eq!(
        text.matches('\n').count(),
        1,
        "a fenced value's embedded newlines must JSON-escape, never split the line: {text:?}"
    );
}

// ── initialize — version negotiation ───────────────────────────────────

#[test]
fn an_unsupported_protocol_version_falls_back_to_the_default() {
    let result = initialize_result(&json!({ "protocolVersion": "2099-01-01" }), INSTRUCTIONS);
    assert_eq!(result["protocolVersion"], DEFAULT_VERSION);
}

#[test]
fn every_supported_older_version_is_echoed_back_verbatim() {
    for v in [
        "2025-11-25",
        "2025-06-18",
        "2025-03-26",
        "2024-11-05",
        "2024-10-07",
    ] {
        let result = initialize_result(&json!({ "protocolVersion": v }), INSTRUCTIONS);
        assert_eq!(result["protocolVersion"], v, "must echo {v}");
    }
}

#[test]
fn a_missing_protocol_version_answers_the_default() {
    let result = initialize_result(&json!({}), INSTRUCTIONS);
    assert_eq!(result["protocolVersion"], DEFAULT_VERSION);
}

#[test]
fn initialize_never_names_2026_07_28() {
    for v in ["2025-11-25", "unknown-future-version"] {
        let result = initialize_result(&json!({ "protocolVersion": v }), INSTRUCTIONS);
        assert_ne!(result["protocolVersion"], "2026-07-28");
    }
}

// ── INSTRUCTIONS / build_instructions (items 7, 14, 18, 24, 27) ─────────

#[test]
fn instructions_name_both_missing_pointer_and_app_closed_and_map_cli_phrasing_onto_tools() {
    assert!(INSTRUCTIONS.contains("app_not_located"));
    assert!(INSTRUCTIONS.contains("app_not_running"));
    assert!(INSTRUCTIONS.contains("call-read"));
    assert!(
        INSTRUCTIONS.contains("--confirm"),
        "must map CLI --confirm phrasing onto this tool's own confirm argument"
    );
}

#[test]
fn instructions_name_connection_lost_alongside_rate_limited_in_the_no_retry_sentence() {
    // item 18 — a payload too large for the bridge frame surfaces as connection_lost, which
    // reads as transient; naming only rate_limited invited a retry loop.
    assert!(INSTRUCTIONS.contains("connection_lost"));
    assert!(INSTRUCTIONS.contains("rate_limited"));
}

/// Every `"error":` STRING LITERAL mcp.rs's own source writes directly — never `agent_call`'s
/// `pub(super)` sentinels (`ERR_UNKNOWN_COMMAND`/`ERR_NOT_EXPOSED`/`ERR_CONFIRMATION_REQUIRED`),
/// referenced by path there and never respelled here. A test-only fixture (item 24): nothing in
/// production reads it, only the two tests below.
const MCP_SENTINELS: &[&str] = &[
    "wrong_tool",
    "result_too_large",
    "server_busy",
    "shutting_down",
];

#[test]
fn instructions_name_every_mcp_only_sentinel() {
    // item 24 — wrong_tool/result_too_large are MCP-only outcomes named nowhere else.
    for sentinel in MCP_SENTINELS {
        assert!(
            INSTRUCTIONS.contains(sentinel),
            "INSTRUCTIONS must name MCP-only sentinel `{sentinel}`"
        );
    }
}

/// Find the next `"error"` key in `source` at or after `from` whose value is a string literal,
/// tolerating ANY amount of whitespace (including a newline, i.e. rustfmt splitting key and value
/// across lines) between `"error"`, `:`, and the opening quote — CodeRabbit, PR #1092: the prior
/// scanner matched only the exact spelling `"error": "` (one space), so `"error":"x"` or a
/// line-split write would silently produce NO match while the "found is non-empty" sanity check
/// stayed green on whatever it DID happen to catch elsewhere in the file. Returns the literal's
/// value and the index just past its closing quote, so the caller can resume scanning from there;
/// a `"error"` occurrence whose value isn't a string (e.g. `"error": some_const`) is skipped, not
/// treated as a scan failure.
fn next_error_literal(source: &str, from: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let mut search_from = from;
    loop {
        let key_pos = source[search_from..].find("\"error\"")?;
        let after_key = search_from + key_pos + "\"error\"".len();
        let mut i = after_key;
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i) != Some(&b':') {
            search_from = after_key;
            continue;
        }
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i) != Some(&b'"') {
            search_from = after_key;
            continue;
        }
        let value_start = i + 1;
        let value_end = value_start + source[value_start..].find('"')?;
        return Some((&source[value_start..value_end], value_end + 1));
    }
}

#[test]
fn every_error_literal_in_mcp_source_is_named_in_mcp_sentinels() {
    // item 24 — a drift guard: every `"error": "..."` string literal this file's own source
    // writes must be a member of MCP_SENTINELS (never `agent_call`'s shared sentinels, which are
    // referenced by path, not respelled here).
    const SOURCE: &str = include_str!("../mcp.rs");
    let mut idx = 0;
    let mut found = Vec::new();
    while let Some((value, next)) = next_error_literal(SOURCE, idx) {
        found.push(value);
        idx = next;
    }
    assert!(
        !found.is_empty(),
        "sanity: the scanner must find at least one literal"
    );
    for f in &found {
        assert!(
            MCP_SENTINELS.contains(f),
            "mcp.rs writes \"error\": \"{f}\" but MCP_SENTINELS doesn't name it"
        );
    }
}

#[test]
fn build_instructions_appends_one_sentence_per_enabled_tier_and_never_duplicates_the_base() {
    let none = build_instructions(Tier::Read);
    let reversible = build_instructions(Tier::Reversible);
    let irreversible = build_instructions(Tier::Irreversible);
    assert_eq!(none, INSTRUCTIONS, "no flags must not append anything");
    assert!(reversible.starts_with(INSTRUCTIONS));
    assert!(reversible.contains("reversible write tier is enabled"));
    assert!(
        !reversible.contains("irreversible tier is enabled"),
        "the irreversible notice must not appear at the reversible tier: {reversible}"
    );
    assert!(irreversible.contains("reversible write tier is enabled"));
    assert!(irreversible.contains("irreversible tier is enabled"));
    assert_eq!(
        irreversible.matches("loopback bridge").count(),
        1,
        "must append, never duplicate, the base INSTRUCTIONS text"
    );
}

#[test]
fn instructions_notices_are_worded_by_tier_not_by_the_literal_flag_typed() {
    // item 27 — launched with ONLY --allow-irreversible; Tier::Irreversible implies the
    // reversible tier too, so BOTH notices append, but neither may claim a flag never typed.
    let text = build_instructions(Tier::from_flags(false, true));
    assert!(
        !text.contains("--allow-reversible") && !text.contains("--allow-irreversible"),
        "notices must be worded by TIER, not by the literal flag: {text}"
    );
    assert!(text.contains("reversible write tier is enabled"));
    assert!(text.contains("irreversible tier is enabled"));
}

// ── curated_tool description join (item 8) ──────────────────────────────

#[test]
fn curated_tool_joins_base_and_extra_as_two_sentences_not_a_run_on() {
    let tool = tools(Tier::Read)
        .into_iter()
        .find(|t| t["name"] == TOOL_BEST_MATCHES)
        .unwrap();
    let description = tool["description"].as_str().unwrap().to_string();
    assert!(
        description.contains(". "),
        "base and extra must be joined as two sentences: {description}"
    );
    assert!(
        !description.contains(") title/company"),
        "must never join with a bare space (the live run-on this fix closed): {description}"
    );
}

#[test]
fn every_scraped_text_tool_carries_the_same_untrusted_fields_notice() {
    // pre-PR gate, extended in review round 2 (MEDIUM — `found-jobs` was added without
    // extending this pairwise check, the exact class of gap `#1088` warns about): every
    // curated tool that returns title/company/location/description scraped text must carry
    // the IDENTICAL notice as `best-matches`, never a fresh one-off pair test per new tool.
    let list = tools(Tier::Read);
    let notice = list
        .iter()
        .find(|t| t["name"] == TOOL_BEST_MATCHES)
        .unwrap()["description"]
        .as_str()
        .unwrap()
        .rsplit_once(". ")
        .unwrap()
        .1
        .to_string();
    for tool in [TOOL_JOB, TOOL_FOUND_JOBS] {
        let description = list.iter().find(|t| t["name"] == tool).unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains(&notice),
            "{tool}'s description must carry the same untrusted-fields notice as best-matches: \
             {description}"
        );
    }
}

// ── Tier (item 21) ───────────────────────────────────────────────────────

#[test]
fn tier_irreversible_always_implies_reversible() {
    assert!(Tier::Irreversible.allows_reversible());
    assert!(Tier::Reversible.allows_reversible());
    assert!(!Tier::Read.allows_reversible());
    assert!(Tier::Irreversible.allows_irreversible());
    assert!(!Tier::Reversible.allows_irreversible());
    assert!(!Tier::Read.allows_irreversible());
}

#[test]
fn from_flags_resolves_every_combination_including_irreversible_alone() {
    assert_eq!(Tier::from_flags(false, false), Tier::Read);
    assert_eq!(Tier::from_flags(true, false), Tier::Reversible);
    assert_eq!(Tier::from_flags(false, true), Tier::Irreversible);
    assert_eq!(Tier::from_flags(true, true), Tier::Irreversible);
}

#[test]
fn server_new_false_true_still_resolves_the_full_irreversible_tier() {
    // The exact gap the review named: nothing previously constructed Server::new(false, true).
    let server = Server::new(false, true);
    assert_eq!(server.tier, Tier::Irreversible);
    assert_eq!(
        names(&server.tools),
        vec![
            "automations",
            "best-matches",
            "call-irreversible",
            "call-read",
            "call-reversible",
            "commands",
            "found-jobs",
            "job",
            "profile",
        ]
    );
}

// ── tools/list — the hand-written literal list, all three launch modes
// (item 11 — mutation-checked by deleting the reversible gate) ─────────

#[test]
fn tool_names_by_launch_mode_match_hand_written_literal_lists() {
    assert_eq!(
        names(&tools(Tier::Read)),
        vec![
            "automations",
            "best-matches",
            "call-read",
            "commands",
            "found-jobs",
            "job",
            "profile"
        ],
        "default server (no flags) must be read tier + commands only"
    );
    assert_eq!(
        names(&tools(Tier::Reversible)),
        vec![
            "automations",
            "best-matches",
            "call-read",
            "call-reversible",
            "commands",
            "found-jobs",
            "job",
            "profile",
        ],
        "--allow-reversible must add exactly call-reversible"
    );
    assert_eq!(
        names(&tools(Tier::Irreversible)),
        vec![
            "automations",
            "best-matches",
            "call-irreversible",
            "call-read",
            "call-reversible",
            "commands",
            "found-jobs",
            "job",
            "profile",
        ],
        "--allow-irreversible must add call-irreversible on top of call-reversible"
    );
}

#[test]
fn calling_the_reversible_tool_without_the_flag_is_invalid_params() {
    let server = Server::new(false, false);
    let mut dispatch = stub_ok;
    let outcome = tool_call_result(
        &json!({
            "name": "call-reversible",
            "arguments": { "namespace": "cli_agents", "command": "cli_agents_redetect" },
        }),
        &server,
        &mut dispatch,
    );
    assert_eq!(outcome.unwrap_err().0, -32602);
}

#[test]
fn calling_the_irreversible_tool_without_the_flag_is_invalid_params() {
    let server = Server::new(false, false);
    let mut dispatch = stub_ok;
    let outcome = tool_call_result(
        &json!({
            "name": "call-irreversible",
            "arguments": { "namespace": "documents", "command": "documents_remove" },
        }),
        &server,
        &mut dispatch,
    );
    assert_eq!(outcome.unwrap_err().0, -32602);
}

#[test]
fn every_curated_tool_and_call_tool_declares_a_bare_object_schema_with_no_ref() {
    for tool in tools(Tier::Irreversible) {
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["type"], "object",
            "{}: root must be type:object",
            tool["name"]
        );
        assert_eq!(
            schema["additionalProperties"], false,
            "{}: additionalProperties must be false",
            tool["name"]
        );
        assert!(schema.get("$ref").is_none(), "{}: no $ref", tool["name"]);
    }
}

#[test]
fn call_irreversible_carries_the_requires_user_interaction_meta() {
    let tool_list = tools(Tier::Irreversible);
    let tool = tool_list
        .iter()
        .find(|t| t["name"] == TOOL_CALL_IRREVERSIBLE)
        .expect("present when allowed");
    assert_eq!(tool["_meta"]["anthropic/requiresUserInteraction"], true);
    assert_eq!(tool["annotations"]["destructiveHint"], true);
}

// ── mcp --help / launch-arg parsing (items 10, 11, 23, 28) ──────────────

#[test]
fn parse_launch_args_accepts_any_subset_of_the_two_flags_in_any_order() {
    assert_eq!(parse_launch_args(&[]).unwrap(), LaunchArgs::default());
    assert_eq!(
        parse_launch_args(&args(&["--allow-reversible"])).unwrap(),
        LaunchArgs {
            help: false,
            allow_reversible: true,
            allow_irreversible: false,
        }
    );
    assert_eq!(
        parse_launch_args(&args(&["--allow-irreversible", "--allow-reversible"])).unwrap(),
        LaunchArgs {
            help: false,
            allow_reversible: true,
            allow_irreversible: true,
        },
        "order must not matter"
    );
}

#[test]
fn parse_launch_args_accepts_help_anywhere_and_rejects_anything_else() {
    assert!(parse_launch_args(&args(&["--help"])).unwrap().help);
    assert!(
        parse_launch_args(&args(&["--allow-reversible", "--help"]))
            .unwrap()
            .help
    );
    assert!(parse_launch_args(&args(&["not-a-flag"])).is_err());
    assert!(parse_launch_args(&args(&["--allow-reversible", "typo"])).is_err());
}

#[test]
fn mcp_help_text_lists_both_flags_and_derives_its_default_list_from_tools() {
    let text = mcp_help_text();
    assert!(text.contains("--allow-reversible"));
    assert!(text.contains("--allow-irreversible"));
    for name in [
        "best-matches",
        "job",
        "profile",
        "automations",
        "commands",
        "call-read",
    ] {
        assert!(text.contains(name), "missing default tool `{name}`: {text}");
    }
    let default_line = text
        .lines()
        .find(|l| l.starts_with("Default"))
        .expect("must have a 'Default (no flags): ...' line");
    assert!(
        !default_line.contains("call-reversible") && !default_line.contains("call-irreversible"),
        "the default-tool-list line must not name a gated tool: {default_line}"
    );
}

#[test]
fn print_help_never_adds_a_trailing_blank_line() {
    let mut buf = Vec::new();
    print_help(&mut buf).unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(
        text.ends_with('\n') && !text.ends_with("\n\n"),
        "must end in exactly one newline, no extra blank line: {text:?}"
    );
}

// ── every POLICY row is served by exactly one call-* tool, or refused
// everywhere if NotExposed (item 12 rewrites the NotExposed branch) ─────

#[test]
fn every_policy_row_is_routed_to_exactly_one_call_tool_or_refused_everywhere_if_not_exposed() {
    for entry in POLICY {
        let (namespace, command) = agent_call::split_path(entry.path);
        let verb = Verb::Call {
            namespace: namespace.to_string(),
            command: command.to_string(),
            input: json!({}),
            confirm: None,
        };
        let accepted_by: Vec<&str> = [TOOL_CALL_READ, TOOL_CALL_REVERSIBLE, TOOL_CALL_IRREVERSIBLE]
            .into_iter()
            .filter(|tool| local_call_refusal(tool, &verb).is_none())
            .collect();
        match entry.effect {
            Effect::NotExposed(_) => assert_eq!(
                accepted_by.len(),
                0,
                "{}: a NotExposed row must refuse locally on every call-* tool (MUST FIX — \
                 review round 2) — got {accepted_by:?}",
                entry.path
            ),
            _ => assert_eq!(
                accepted_by.len(),
                1,
                "{}: exactly one call-* tool must accept this row locally — got {accepted_by:?}",
                entry.path
            ),
        }
    }
}

#[test]
fn extension_bridge_status_the_token_row_refuses_locally_on_every_call_tool() {
    // The HIGH-1 row: it returns the plaintext pairing token verbatim. A cross-version peer (an
    // updater-staged newer exe, an older still-running paired app) must not be the only thing
    // standing between this row and an MCP caller — refuse it locally too, no bridge involved.
    let verb = Verb::Call {
        namespace: "extension_bridge".to_string(),
        command: "extension_bridge_status".to_string(),
        input: json!({}),
        confirm: None,
    };
    for tool in [TOOL_CALL_READ, TOOL_CALL_REVERSIBLE, TOOL_CALL_IRREVERSIBLE] {
        let refusal = local_call_refusal(tool, &verb).expect("must refuse locally on every tool");
        assert_eq!(
            refusal["error"],
            crate::extension_bridge::agent_call::ERR_NOT_EXPOSED
        );
        assert!(refusal["detail"]
            .as_str()
            .unwrap()
            .contains("pairing token"));
    }
}

// ── MUST FIX — unknown_command / wrong_tool local refusals ──────────────

#[test]
fn call_read_refuses_a_namespace_command_the_local_policy_does_not_know() {
    let verb = Verb::Call {
        namespace: "nope".to_string(),
        command: "delete_everything".to_string(),
        input: json!({}),
        confirm: None,
    };
    let refusal = local_call_refusal(TOOL_CALL_READ, &verb).expect("must refuse");
    assert_eq!(refusal["dispatched"], false);
    assert_eq!(refusal["error"], agent_call::ERR_UNKNOWN_COMMAND);
}

#[test]
fn call_read_refuses_a_real_reversible_row_naming_the_right_tool() {
    // `cli_agents_redetect` is a real Reversible POLICY row.
    let verb = Verb::Call {
        namespace: "cli_agents".to_string(),
        command: "cli_agents_redetect".to_string(),
        input: json!({}),
        confirm: None,
    };
    let refusal = local_call_refusal(TOOL_CALL_READ, &verb).expect("must refuse — wrong tool");
    assert_eq!(refusal["error"], "wrong_tool");
    assert!(refusal["detail"]
        .as_str()
        .unwrap()
        .contains(TOOL_CALL_REVERSIBLE));
}

#[test]
fn call_read_accepts_a_real_read_row() {
    let verb = Verb::Call {
        namespace: "cli_agents".to_string(),
        command: "cli_agents_status".to_string(),
        input: json!({}),
        confirm: None,
    };
    assert!(local_call_refusal(TOOL_CALL_READ, &verb).is_none());
}

// ── confirm is passed through verbatim on call-irreversible only ────────

#[test]
fn confirm_reaches_the_verb_only_via_the_irreversible_tool() {
    let arguments =
        json!({ "namespace": "documents", "command": "documents_remove", "confirm": "Resume A" });
    let argv = tool_argv(TOOL_CALL_IRREVERSIBLE, &arguments);
    let verb = parse_verb(&argv).unwrap();
    assert_eq!(
        verb,
        Verb::Call {
            namespace: "documents".to_string(),
            command: "documents_remove".to_string(),
            input: json!({}),
            confirm: Some("Resume A".to_string()),
        }
    );
}

#[test]
fn a_confirm_argument_sent_to_call_read_is_silently_ignored() {
    let arguments =
        json!({ "namespace": "jobs", "command": "jobs_list", "confirm": "should never forward" });
    let argv = tool_argv(TOOL_CALL_READ, &arguments);
    let verb = parse_verb(&argv).unwrap();
    assert_eq!(
        verb,
        Verb::Call {
            namespace: "jobs".to_string(),
            command: "jobs_list".to_string(),
            input: json!({}),
            confirm: None,
        }
    );
}

// ── found-jobs tool_argv mapping (MEDIUM fix, review round 2 — this new arm had no
// coverage at all) ───────────────────────────────────────────────────────────────

#[test]
fn found_jobs_tool_argv_maps_autopilot_id_limit_and_cursor() {
    let arguments = json!({ "autopilotId": "ap-1", "limit": 10, "cursor": "20" });
    let argv = tool_argv(TOOL_FOUND_JOBS, &arguments);
    assert_eq!(
        parse_verb(&argv).unwrap(),
        Verb::FoundJobs {
            autopilot_id: "ap-1".to_string(),
            limit: Some(10),
            cursor: Some("20".to_string()),
        }
    );
}

#[test]
fn found_jobs_tool_argv_omits_optional_flags_when_absent() {
    let argv = tool_argv(TOOL_FOUND_JOBS, &json!({ "autopilotId": "ap-1" }));
    assert_eq!(
        parse_verb(&argv).unwrap(),
        Verb::FoundJobs {
            autopilot_id: "ap-1".to_string(),
            limit: None,
            cursor: None,
        }
    );
}

/// HIGH fix, review round 2 — a JSON NUMBER `cursor` (as a real MCP client would send,
/// since the declared schema type is `string` but nothing on the wire enforces that) must
/// still reach `parse_verb` as a string, not be dropped as if the caller had sent nothing.
#[test]
fn found_jobs_tool_argv_forwards_a_numeric_cursor_rather_than_dropping_it() {
    let arguments = json!({ "autopilotId": "ap-1", "cursor": 100 });
    let argv = tool_argv(TOOL_FOUND_JOBS, &arguments);
    assert_eq!(
        parse_verb(&argv).unwrap(),
        Verb::FoundJobs {
            autopilot_id: "ap-1".to_string(),
            limit: None,
            cursor: Some("100".to_string()),
        }
    );
}

#[test]
fn found_jobs_tool_argv_treats_an_explicit_null_cursor_as_absent() {
    let arguments = json!({ "autopilotId": "ap-1", "cursor": null });
    let argv = tool_argv(TOOL_FOUND_JOBS, &arguments);
    assert_eq!(
        parse_verb(&argv).unwrap(),
        Verb::FoundJobs {
            autopilot_id: "ap-1".to_string(),
            limit: None,
            cursor: None,
        }
    );
}

#[test]
fn confirmation_required_result_carries_the_cli_payload_verbatim_plus_one_note() {
    let payload = json!({
        "dispatched": false, "namespace": "ai", "command": "ai_set_provider_key",
        "error": agent_call::ERR_CONFIRMATION_REQUIRED,
        "detail": "read `agent call ai:ai_has_provider_key` and pass its own `has` field as --confirm",
    });
    let result = tool_result(payload.clone(), 4);
    assert_eq!(result["isError"], true);
    let blocks = result["content"].as_array().unwrap();
    assert_eq!(
        blocks[0]["text"],
        payload.to_string(),
        "content[0] must be the CLI payload byte-for-byte"
    );
    assert_eq!(blocks[1]["text"], "exitCode: 4");
    assert!(
        blocks.len() >= 3,
        "a confirmation_required result must carry a third, mapping block"
    );
    assert!(blocks[2]["text"].as_str().unwrap().contains("call-read"));
}

// ── structuredContent is dropped everywhere (item 15) ───────────────────

#[test]
fn tool_result_never_carries_structured_content() {
    let result = tool_result(json!({ "ok": true }), 0);
    assert!(
        result.get("structuredContent").is_none(),
        "structuredContent must never appear — see the module doc's output-contract section"
    );
}

// ── result-size cap, checked in tool_result itself (items 13, 16, 17, 22, 25) ──

#[test]
fn oversized_result_detail_never_names_the_cli_invocation() {
    let refusal = oversized_result(MCP_RESULT_MAX_BYTES + 1);
    let detail = refusal["detail"].as_str().unwrap();
    assert!(
        !detail.contains("agent call") && !detail.contains("agent mcp"),
        "must not hand the model a bypass recipe: {detail}"
    );
    assert_eq!(
        refusal["dispatched"], false,
        "must mirror every other Verb::Call refusal's own shape"
    );
}

#[test]
fn a_dispatched_payload_over_the_byte_cap_refuses_and_never_truncates() {
    let server = Server::new(true, true);
    let huge =
        json!({ "ok": true, "resource": "call", "blob": "x".repeat(MCP_RESULT_MAX_BYTES + 10) });
    let mut dispatch = move |_: &Verb| Ok(huge.clone());
    let outcome = tool_call_result(
        &json!({
            "name": "call-read",
            "arguments": { "namespace": "commands", "command": "documents_export_document" },
        }),
        &server,
        &mut dispatch,
    )
    .unwrap();
    assert_eq!(outcome["isError"], true);
    let text = outcome["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).expect("must stay valid JSON — never truncated");
    assert_eq!(parsed["error"], "result_too_large");
    assert_eq!(
        parsed["dispatched"], false,
        "must mirror every other Verb::Call refusal's own shape"
    );
    assert!(parsed["bytes"].as_u64().unwrap() > MCP_RESULT_MAX_BYTES as u64);
    let detail = parsed["detail"].as_str().unwrap();
    assert!(
        !detail.contains("agent call"),
        "must never hand the model a bypass recipe: {detail}"
    );
    assert_eq!(outcome["content"][1]["text"], "exitCode: 2");
}

#[test]
fn a_locally_refused_oversized_namespace_never_gets_echoed_back_in_full() {
    // item 17 — a local refusal (unknown_command here) used to return BEFORE any cap check, so an
    // oversized caller-chosen `namespace` reproduced the exact frame size the cap exists to bound.
    let server = Server::new(true, true);
    let huge_namespace = "n".repeat(MCP_RESULT_MAX_BYTES + 10);
    let mut dispatch = stub_ok;
    let outcome = tool_call_result(
        &json!({
            "name": "call-read",
            "arguments": { "namespace": huge_namespace, "command": "whatever" },
        }),
        &server,
        &mut dispatch,
    )
    .unwrap();
    assert_eq!(outcome["isError"], true);
    let text = outcome["content"][0]["text"].as_str().unwrap();
    assert!(
        text.len() < MCP_RESULT_MAX_BYTES,
        "must refuse instead of echoing the oversized namespace back verbatim: {} bytes",
        text.len()
    );
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["error"], "result_too_large");
}

// ── commands (local, no bridge) ──────────────────────────────────────────

#[test]
fn commands_filters_by_effect_and_never_touches_the_bridge() {
    let all = commands_value(&json!({}), Tier::Irreversible);
    let read_only = commands_value(&json!({ "effect": "read" }), Tier::Irreversible);
    let all_rows = all["commands"].as_array().unwrap().len();
    let read_rows = read_only["commands"].as_array().unwrap().len();
    assert!(read_rows > 0 && read_rows < all_rows);
    for row in read_only["commands"].as_array().unwrap() {
        assert_eq!(row["effect"], "read");
        assert_eq!(row["tool"], TOOL_CALL_READ);
    }
}

#[test]
fn commands_names_the_right_tool_for_every_effect_class_with_all_flags_enabled() {
    // Pins the SECOND copy of the Effect→tool mapping (`tool_for`, used by both `commands_value`
    // and `local_call_refusal`) — the first copy was already covered by the `read`-only assertion
    // above, but nothing previously pinned `reversible`/`irreversible`/`not_exposed`.
    let all = commands_value(&json!({}), Tier::Irreversible);
    for row in all["commands"].as_array().unwrap() {
        match row["effect"].as_str().unwrap() {
            "read" => assert_eq!(row["tool"], TOOL_CALL_READ),
            "reversible" => assert_eq!(row["tool"], TOOL_CALL_REVERSIBLE),
            "irreversible" => assert_eq!(row["tool"], TOOL_CALL_IRREVERSIBLE),
            "not_exposed" => assert!(row.get("tool").is_none() && row.get("unavailable").is_none()),
            other => panic!("unexpected effect {other}"),
        }
    }
}

#[test]
fn commands_marks_irreversible_rows_unavailable_without_the_irreversible_flag() {
    let out = commands_value(&json!({ "effect": "irreversible" }), Tier::Reversible);
    let rows = out["commands"].as_array().unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        assert!(
            row.get("tool").is_none(),
            "must not name a gated tool: {row}"
        );
        assert_eq!(
            row["unavailable"],
            "server started without --allow-irreversible"
        );
    }
}

#[test]
fn commands_marks_reversible_rows_unavailable_without_the_reversible_flag() {
    let out = commands_value(&json!({ "effect": "reversible" }), Tier::Read);
    let rows = out["commands"].as_array().unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        assert!(
            row.get("tool").is_none(),
            "must not name a gated tool: {row}"
        );
        assert_eq!(
            row["unavailable"],
            "server started without --allow-reversible"
        );
    }
}

#[test]
fn commands_with_an_unknown_effect_value_is_a_usage_error_not_a_silent_empty_success() {
    let server = Server::new(true, true);
    let mut dispatch = stub_ok;
    let outcome = tool_call_result(
        &json!({ "name": "commands", "arguments": { "effect": "bogus" } }),
        &server,
        &mut dispatch,
    )
    .unwrap();
    assert_eq!(
        outcome["isError"], true,
        "an unknown effect must not read as success"
    );
    assert_eq!(outcome["content"][1]["text"], "exitCode: 2");
    let text = outcome["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["error"], "usage");
}

#[test]
fn commands_with_a_non_string_effect_value_is_a_usage_error_for_every_json_type() {
    // item 20 — {"effect":5} (and bool/null/object) used to skip the old
    // `.and_then(Value::as_str)` gate entirely and answer with every row, isError:false.
    let server = Server::new(true, true);
    for bad in [json!(5), json!(true), Value::Null, json!({"nested": 1})] {
        let mut dispatch = stub_ok;
        let outcome = tool_call_result(
            &json!({ "name": "commands", "arguments": { "effect": bad.clone() } }),
            &server,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(
            outcome["isError"], true,
            "effect={bad:?} must not read as success"
        );
        let text = outcome["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["error"], "usage", "effect={bad:?}");
    }
}

#[test]
fn commands_names_the_proof_source_for_an_irreversible_row() {
    let out = commands_value(&json!({ "effect": "irreversible" }), Tier::Irreversible);
    let row = out["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["command"] == "ai_set_provider_key")
        .expect("ai_set_provider_key is a real Irreversible row");
    assert_eq!(row["proofFrom"], "ai:ai_has_provider_key");
    assert_eq!(row["proofInput"], "provider");
    assert!(
        row.get("proofInputValue").is_none(),
        "a FromCaller value is the caller's own input and must never be echoed: {row}"
    );
}

#[test]
fn commands_names_the_literal_proof_input_value_for_privacy_sign_out_all() {
    let out = commands_value(&json!({ "effect": "irreversible" }), Tier::Irreversible);
    let row = out["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["command"] == "privacy_sign_out_all")
        .expect("privacy_sign_out_all is a real Irreversible row with a Literal proof input");
    assert_eq!(row["proofInput"], "boardId");
    assert_eq!(
        row["proofInputValue"], "linkedin",
        "a Literal input's value is not secret and is the one thing this ceremony can't \
         otherwise complete from `commands` alone"
    );
}

// ── source hygiene guard (mutation-visible: adding any of these turns
// this test red immediately) ────────────────────────────────────────────

#[test]
fn mcp_source_never_prints_or_pretty_prints() {
    const SOURCE: &str = include_str!("../mcp.rs");
    for banned in ["println!(", "print!(", "to_string_pretty(", "eprintln!("] {
        assert!(
            !SOURCE.contains(banned),
            "mcp.rs must never call {banned} — see emit()'s own doc"
        );
    }
    assert_eq!(
        SOURCE.matches("stdout()").count(),
        1,
        "mcp.rs must call stdout() exactly once — see emit()'s own doc"
    );
}
