use super::*;
use crate::scraping::types::WorkType;

#[test]
fn test_default_top_n() {
    assert_eq!(default_top_n(), 3);
}

#[test]
fn test_autopilot_status_partial_eq() {
    assert_eq!(AutopilotStatus::Active, AutopilotStatus::Active);
    assert_ne!(AutopilotStatus::Active, AutopilotStatus::Paused);
}

#[test]
fn test_autopilot_target_serialization() {
    let target = AutopilotTarget {
        boards: vec!["linkedin".to_string()],
        query: "software engineer".to_string(),
        location: Some("Berlin".to_string()),
        country_code: None,
        work_types: None,
        pages: 5,
        date_filter: None,
        top_n: 3,
        watched_companies_only: None,
    };
    let json = serde_json::to_string(&target);
    assert!(json.is_ok());
}

// ── `parse_work_types_lenient` — the only persisted, data-loss-capable path ──
//
// MUTATION CHECK for all four tests below: replace the `work_types` field's
// attributes with the naive
// `#[serde(default)] pub work_types: Option<Vec<crate::scraping::types::WorkType>>`
// (delete `deserialize_with = "parse_work_types_lenient"`). Every one of these
// tests goes red — `work_types_mixed_validity_drops_bad_entry_keeps_boards_and_query`
// because stock `Vec<WorkType>::deserialize` fails the WHOLE array (and so the
// whole `from_value::<AutopilotTarget>` call) on the one unrecognised entry
// instead of dropping it, which is exactly the data-loss path
// `AutopilotStore::load` → `save()` this field's doc warns about. Restore
// after checking.

#[test]
fn work_types_mixed_validity_drops_bad_entry_keeps_boards_and_query() {
    let raw = serde_json::json!({
        "boards": ["linkedin"],
        "query": "rust",
        "pages": 1,
        "workTypes": ["remote", "bogus", "hybrid"],
    });
    let target: AutopilotTarget = serde_json::from_value(raw)
        .expect("a single unrecognised workTypes entry must not fail the whole target");
    assert_eq!(target.boards, vec!["linkedin".to_string()]);
    assert_eq!(target.query, "rust");
    assert_eq!(
        target.work_types,
        Some(vec![WorkType::Remote, WorkType::Hybrid]),
        "the bad entry is dropped, the valid ones kept in order"
    );
}

#[test]
fn work_types_absent_key_deserializes_to_none() {
    // Every autopilot on disk today has no `workTypes` key at all (the only
    // UI control that could set it was removed in PR #614 before this field
    // existed) — this is the common case, not an edge case.
    let raw = serde_json::json!({
        "boards": ["linkedin"],
        "query": "rust",
        "pages": 1,
    });
    let target: AutopilotTarget = serde_json::from_value(raw)
        .expect("a legacy record with no workTypes key must deserialize");
    assert_eq!(target.work_types, None);
}

#[test]
fn work_types_all_unrecognised_collapses_to_empty_vec_not_none() {
    // `Some(vec![])`, not `None` — the deserializer's job is only to drop bad
    // entries, not to decide "empty means no filter"; that collapse belongs to
    // `BoardSearchInput::work_type_spec`, the single seam every consumer reads.
    let raw = serde_json::json!({
        "boards": ["linkedin"],
        "query": "rust",
        "pages": 1,
        "workTypes": ["bogus", "also-bogus"],
    });
    let target: AutopilotTarget =
        serde_json::from_value(raw).expect("an all-unrecognised array must still deserialize");
    assert_eq!(target.work_types, Some(Vec::new()));
}

#[test]
fn work_types_roundtrips_through_serialize_deserialize() {
    let target = AutopilotTarget {
        boards: vec!["linkedin".to_string()],
        query: "rust".to_string(),
        location: None,
        country_code: None,
        work_types: Some(vec![WorkType::OnSite, WorkType::Hybrid]),
        pages: 1,
        date_filter: None,
        top_n: default_top_n(),
        watched_companies_only: None,
    };
    let json = serde_json::to_value(&target).unwrap();
    assert_eq!(
        json.get("workTypes"),
        Some(&serde_json::json!(["on-site", "hybrid"])),
        "must serialize through the shared kebab-case WorkType vocabulary"
    );
    let back: AutopilotTarget = serde_json::from_value(json).unwrap();
    assert_eq!(
        back.work_types,
        Some(vec![WorkType::OnSite, WorkType::Hybrid])
    );
}

#[test]
fn test_autopilot_filter_serialization() {
    let filter = AutopilotFilter {
        min_match_score: 75.0,
        keywords: Some(vec!["rust".to_string(), "typescript".to_string()]),
        exclude_keywords: None,
    };
    let json = serde_json::to_string(&filter);
    assert!(json.is_ok());
}

#[test]
fn legacy_record_without_assistant_fields_defaults_to_disabled() {
    // An autopilots.json written before Phase 4 has no `assistant*` keys — it must
    // load with AI notes OFF and no provider snapshot (opt-in, zero surprise).
    let json = serde_json::json!({
        "_id": "ap1",
        "name": "Legacy",
        "status": "active",
        "target": { "boards": ["linkedin"], "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "daily",
        "totalFound": 0,
        "totalApplied": 0,
        "createdAt": 1,
        "updatedAt": 1
    });
    let ap: Autopilot = serde_json::from_value(json).expect("legacy record must deserialize");
    assert!(
        !ap.assistant,
        "AI notes must default OFF for a legacy record"
    );
    assert!(ap.assistant_provider.is_none());
    assert!(ap.assistant_model.is_none());
    assert!(ap.assistant_base_url.is_none());
}

#[test]
fn legacy_found_job_without_assistant_notes_deserializes_to_none() {
    // A found job persisted before Phase 4 has no `assistantNotes` key.
    let json = serde_json::json!({
        "title": "Engineer",
        "company": "Acme",
        "url": "https://acme.example/1",
        "foundAt": 1u64
    });
    let job: FoundJob = serde_json::from_value(json).expect("legacy found job must deserialize");
    assert!(job.assistant_notes.is_none());
}

#[test]
fn assistant_note_round_trips_on_a_found_job() {
    // A note set by the AI-notes step survives serialize→deserialize (persisted on
    // the record, surfaced to the renderer under `assistantNotes`).
    let mut job = found_job("https://acme.example/2", 5);
    job.assistant_notes = Some("Strong Rust fit; tailor the systems-design bullet.".into());
    let round: FoundJob = serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
    assert_eq!(
        round.assistant_notes.as_deref(),
        Some("Strong Rust fit; tailor the systems-design bullet.")
    );
}

#[test]
fn test_str_field() {
    let value = serde_json::json!({ "name": "Test", "other": "Value" });
    assert_eq!(str_field(&value, "name"), "Test");
    assert_eq!(str_field(&value, "missing"), "");
}

#[test]
fn test_now_ms() {
    let now = now_ms();
    assert!(now > 0);
}

#[test]
fn legacy_auto_apply_records_load_and_strip_dead_keys_on_save() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let dir = temp.path().to_path_buf();

    // A record persisted before the auto-apply engine was removed: it still
    // carries the now-dropped `action` / `autoSubmit` keys. Loading must not
    // fail (serde ignores unknown fields) — the silent find-&-save migration.
    let legacy = r#"[{
        "_id": "ap-legacy",
        "name": "Legacy AP",
        "status": "active",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "action": "auto_apply",
        "schedule": "daily",
        "autoSubmit": true,
        "coverLetter": "Dear team",
        "totalFound": 4,
        "totalApplied": 2,
        "foundJobs": [],
        "createdAt": 1,
        "updatedAt": 1
    }]"#;
    std::fs::write(dir.join("autopilots.json"), legacy).unwrap();

    let store = AutopilotStore::new(&dir);
    let list = store.list();
    assert_eq!(list.len(), 1, "legacy record loads despite dropped keys");
    let ap = &list[0];
    assert_eq!(ap.id, "ap-legacy");
    assert_eq!(ap.schedule, "daily");
    assert_eq!(ap.status, AutopilotStatus::Active);
    assert_eq!(ap.cover_letter.as_deref(), Some("Dear team"));

    // Touching the record rewrites the file from the new struct — the dead
    // auto-apply keys are gone from disk going forward.
    store.stamp_last_run("ap-legacy");
    let on_disk = std::fs::read_to_string(dir.join("autopilots.json")).unwrap();
    assert!(!on_disk.contains("\"action\""), "action stripped on save");
    assert!(
        !on_disk.contains("autoSubmit"),
        "autoSubmit stripped on save"
    );
    assert!(
        on_disk.contains("Dear team"),
        "kept fields survive the rewrite"
    );
}

#[test]
fn load_drops_only_the_unparseable_record_not_the_whole_file() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let dir = temp.path().to_path_buf();

    // One good record + one record whose `runStatus` is a variant this build's
    // `RunStatus` enum doesn't know (e.g. a downgrade after a future release
    // added one) — before the per-record tolerant load, ANY record failing
    // `Vec<Autopilot>` deserialization failed the WHOLE file parse, producing
    // an empty map; a later `save()` would then silently overwrite the file
    // and lose every other (perfectly valid) record too.
    let mixed = r#"[
        {
            "_id": "ap-good",
            "name": "Good AP",
            "status": "active",
            "target": { "board": "linkedin", "query": "rust", "pages": 1 },
            "filter": { "minMatchScore": 50.0 },
            "schedule": "daily",
            "totalFound": 0,
            "totalApplied": 0,
            "createdAt": 1,
            "updatedAt": 1
        },
        {
            "_id": "ap-future",
            "name": "Future AP",
            "status": "active",
            "target": { "board": "linkedin", "query": "rust", "pages": 1 },
            "filter": { "minMatchScore": 50.0 },
            "schedule": "daily",
            "runStatus": "someFutureStatus",
            "totalFound": 0,
            "totalApplied": 0,
            "createdAt": 1,
            "updatedAt": 1
        }
    ]"#;
    std::fs::write(dir.join("autopilots.json"), mixed).unwrap();

    let store = AutopilotStore::new(&dir);
    let list = store.list();
    assert_eq!(
        list.len(),
        1,
        "the good record loads; only the unparseable one is dropped"
    );
    assert_eq!(list[0].id, "ap-good");

    // A post-tolerant-load save must not lose data on disk. `create()` reads
    // via the same `load()` (tolerant-parsed, already dropped "ap-future") and
    // then `save()`s the FULL in-memory map back — if that save ever wrote only
    // the newly-touched record instead of the whole map, "ap-good" would
    // silently vanish from disk the moment anything else was created.
    store.create(serde_json::json!({
        "name": "New AP",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "manual",
    }));

    // A FRESH store over the same dir has an empty cache, so its `list()` call
    // re-reads and re-parses what actually landed on disk — not the in-memory
    // cache the original `store` still holds. This is the real proof that the
    // save after a tolerant load didn't drop data.
    let fresh_store = AutopilotStore::new(&dir);
    let fresh_list = fresh_store.list();
    assert_eq!(
        fresh_list.len(),
        2,
        "both the surviving original record and the newly created one must be on disk"
    );
    assert!(
        fresh_list.iter().any(|a| a.id == "ap-good"),
        "the original good record must survive a post-tolerant-load save, not just the in-memory read"
    );
    assert!(
        fresh_list.iter().any(|a| a.name == "New AP"),
        "the newly created record must also be present"
    );
}

#[test]
fn test_u32_field_in_range_rejects_out_of_range_and_non_numeric() {
    let v = serde_json::json!({
        "good": 23,
        "tooBig": 25,
        "minOk": 59,
        "minBad": 60,
        "negative": -1,
        "text": "9",
    });
    // In-range values pass through.
    assert_eq!(u32_field_in_range(&v, "good", 23), Some(23));
    assert_eq!(u32_field_in_range(&v, "minOk", 59), Some(59));
    // Out-of-range / non-numeric / absent → None (falls back to scheduler default).
    assert_eq!(u32_field_in_range(&v, "tooBig", 23), None);
    assert_eq!(u32_field_in_range(&v, "minBad", 59), None);
    assert_eq!(u32_field_in_range(&v, "negative", 23), None);
    assert_eq!(u32_field_in_range(&v, "text", 23), None);
    assert_eq!(u32_field_in_range(&v, "missing", 23), None);
}

#[test]
fn create_drops_out_of_range_schedule_time_so_scheduler_falls_back() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());

    // A client that bypassed the Zod range check sends scheduleHour: 25 /
    // scheduleMinute: 60. Persisting those verbatim would make `local_at`
    // return None forever → the autopilot is silently never due. Instead the
    // storage boundary stores None, so the scheduler uses its safe default.
    let ap = store.create(serde_json::json!({
        "name": "Out of range",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "daily",
        "scheduleHour": 25,
        "scheduleMinute": 60,
    }));
    assert_eq!(ap.schedule_hour, None, "out-of-range hour is not persisted");
    assert_eq!(
        ap.schedule_minute, None,
        "out-of-range minute is not persisted"
    );

    // A valid time is kept as-is.
    let ok = store.create(serde_json::json!({
        "name": "Valid",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "daily",
        "scheduleHour": 18,
        "scheduleMinute": 30,
    }));
    assert_eq!(ok.schedule_hour, Some(18));
    assert_eq!(ok.schedule_minute, Some(30));
}

#[test]
fn update_rejects_out_of_range_time_while_keeping_null_clear() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "AP",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "daily",
        "scheduleHour": 10,
        "scheduleMinute": 15,
    }));

    // Patching with an out-of-range hour clears it to None rather than poisoning.
    let patched = store
        .update(&ap.id, serde_json::json!({ "scheduleHour": 99 }))
        .unwrap();
    assert_eq!(patched.schedule_hour, None, "out-of-range patch → None");
    assert_eq!(patched.schedule_minute, Some(15), "untouched field kept");

    // Explicit null still clears (existing behavior preserved).
    let cleared = store
        .update(&ap.id, serde_json::json!({ "scheduleMinute": null }))
        .unwrap();
    assert_eq!(cleared.schedule_minute, None, "explicit null clears");
}

#[test]
fn update_toggling_assistant_off_clears_the_provider_snapshot() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "AP",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "daily",
        "assistant": true,
        "assistantProvider": "openai",
        "assistantModel": "gpt-4o",
        "assistantBaseUrl": "https://api.openai.com",
    }));
    assert!(ap.assistant);
    assert_eq!(ap.assistant_provider.as_deref(), Some("openai"));
    assert_eq!(ap.assistant_model.as_deref(), Some("gpt-4o"));
    assert_eq!(
        ap.assistant_base_url.as_deref(),
        Some("https://api.openai.com")
    );

    // The renderer omits assistantProvider/Model/BaseUrl when toggling off, so a
    // patch with only `assistant: false` must clear all three itself.
    let updated = store
        .update(&ap.id, serde_json::json!({ "assistant": false }))
        .unwrap();
    assert!(!updated.assistant);
    assert!(
        updated.assistant_provider.is_none(),
        "stale provider snapshot must be cleared on toggle-off"
    );
    assert!(
        updated.assistant_model.is_none(),
        "stale model snapshot must be cleared on toggle-off"
    );
    assert!(
        updated.assistant_base_url.is_none(),
        "stale base-url snapshot must be cleared on toggle-off"
    );

    // Re-enabling with a fresh snapshot still sets it (the enable path is intact).
    let reenabled = store
        .update(
            &ap.id,
            serde_json::json!({
                "assistant": true,
                "assistantProvider": "anthropic",
                "assistantModel": "claude-3-5-sonnet",
            }),
        )
        .unwrap();
    assert!(reenabled.assistant);
    assert_eq!(reenabled.assistant_provider.as_deref(), Some("anthropic"));
    assert_eq!(
        reenabled.assistant_model.as_deref(),
        Some("claude-3-5-sonnet")
    );
}

#[test]
fn test_clear_all_removes_every_autopilot() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    for name in ["AP1", "AP2"] {
        store.create(serde_json::json!({
            "name": name,
            "target": { "board": "linkedin", "query": "rust", "pages": 1 },
            "filter": { "minMatchScore": 50.0 },
            "schedule": "manual",
        }));
    }
    assert_eq!(store.list().len(), 2);

    store.clear_all();
    assert!(store.list().is_empty());
}

#[test]
fn mark_interrupted_runs_flips_only_in_progress() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());

    let make = |name: &str| {
        store.create(serde_json::json!({
            "name": name,
            "target": { "board": "linkedin", "query": "rust", "pages": 1 },
            "filter": { "minMatchScore": 50.0 },
            "schedule": "manual",
        }))
    };
    let running = make("running");
    let done = make("done");

    store.set_run_status(&running.id, RunStatus::InProgress);
    store.set_run_status(&done.id, RunStatus::Completed);

    let reconciled = store.mark_interrupted_runs();
    assert_eq!(
        reconciled.len(),
        1,
        "only the in-progress run is reconciled"
    );
    assert_eq!(
        reconciled[0], running.id,
        "the reconciled id is the interrupted run's, so the scheduler retries the right one"
    );

    let status = |id: &str| store.get(id).unwrap().run_status;
    assert_eq!(status(&running.id), Some(RunStatus::Interrupted));
    assert_eq!(status(&done.id), Some(RunStatus::Completed));

    // Idempotent: a second startup sweep finds nothing to reconcile.
    assert!(store.mark_interrupted_runs().is_empty());
}

#[test]
fn record_run_marks_the_run_completed() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "ap",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "manual",
    }));
    store.set_run_status(&ap.id, RunStatus::InProgress);

    // A run with no per-board problems (empty summaries) records a clean
    // `Completed` — preserving the pre-summaries behavior.
    store.record_run(&ap.id, 3, 0, Vec::new(), Vec::new(), &no_tombstones(), &[]);
    assert_eq!(
        store.get(&ap.id).unwrap().run_status,
        Some(RunStatus::Completed)
    );
}

/// Build a per-board summary for the status-derivation tests. `count` is only
/// nonzero for the "succeeded" cases so a reader can tell them apart.
fn board_summary(
    board: &str,
    count: usize,
    error: Option<&str>,
    skipped: Option<&str>,
    truncated: Option<&str>,
) -> crate::scraping::BoardScrapeSummary {
    crate::scraping::BoardScrapeSummary {
        board: board.into(),
        count,
        error: error.map(String::from),
        skipped: skipped.map(String::from),
        truncated: truncated.map(String::from),
        notes: Vec::new(),
        health: None,
    }
}

#[test]
fn derive_run_status_maps_summaries_to_honest_outcome() {
    // No boards reported anything → nothing failed, so a clean completion.
    assert_eq!(derive_run_status(&[]), RunStatus::Completed);

    // Every board returned results, no errors/truncation → Completed.
    assert_eq!(
        derive_run_status(&[
            board_summary("greenhouse", 4, None, None, None),
            board_summary("lever", 2, None, None, None),
        ]),
        RunStatus::Completed
    );

    // A board that RAN clean and genuinely found zero (no error, no skip, no
    // truncation) is a real "no jobs today", not a failure — Completed, not
    // Failed. This is the core case the audit called out: "no jobs matched"
    // must stay distinguishable from "everything failed".
    assert_eq!(
        derive_run_status(&[board_summary("greenhouse", 0, None, None, None)]),
        RunStatus::Completed
    );

    // At least one succeeded + at least one errored → CompletedWithErrors.
    assert_eq!(
        derive_run_status(&[
            board_summary("greenhouse", 4, None, None, None),
            board_summary("aggregator", 0, Some("429 Too Many Requests"), None, None),
        ]),
        RunStatus::CompletedWithErrors
    );

    // A partial (truncated) harvest is a partial success → CompletedWithErrors,
    // even when it is the ONLY board.
    assert_eq!(
        derive_run_status(&[board_summary(
            "themuse",
            10,
            None,
            None,
            Some("page 2 of 5 failed: HTTP 429"),
        )]),
        RunStatus::CompletedWithErrors
    );

    // Zero boards succeeded (all errored) → Failed.
    assert_eq!(
        derive_run_status(&[
            board_summary(
                "aggregator",
                0,
                Some("credential store unavailable"),
                None,
                None
            ),
            board_summary("linkedin", 0, Some("blocked"), None, None),
        ]),
        RunStatus::Failed
    );

    // Zero boards ran because all were skipped → Failed (nothing actually ran,
    // so the run produced nothing — an honest failure, not a clean zero).
    assert_eq!(
        derive_run_status(&[board_summary(
            "aggregator",
            0,
            None,
            Some("needs-keys"),
            None
        )]),
        RunStatus::Failed
    );

    // A skip alongside a real success does NOT downgrade — a skipped board is an
    // expected no-op, not a failure of a board that ran.
    assert_eq!(
        derive_run_status(&[
            board_summary("greenhouse", 3, None, None, None),
            board_summary("linkedin", 0, None, Some("needs-login"), None),
        ]),
        RunStatus::Completed
    );
}

#[test]
fn record_run_persists_summaries_and_derives_status() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "ap",
        "target": { "board": "aggregator", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));

    // One board delivered, one errored → the record must say CompletedWithErrors
    // and keep BOTH summaries so the UI can explain the shortfall later.
    let summaries = vec![
        board_summary("greenhouse", 2, None, None, None),
        board_summary("aggregator", 0, Some("429 Too Many Requests"), None, None),
    ];
    store.record_run(&ap.id, 2, 0, Vec::new(), summaries, &no_tombstones(), &[]);

    let reloaded = store.get(&ap.id).unwrap();
    assert_eq!(reloaded.run_status, Some(RunStatus::CompletedWithErrors));
    assert_eq!(reloaded.last_run_summaries.len(), 2);
    assert_eq!(
        reloaded.last_run_summaries[1].error.as_deref(),
        Some("429 Too Many Requests")
    );
}

#[test]
fn record_run_strips_board_health_so_it_never_reaches_a_backup() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "ap",
        "target": { "board": "aggregator", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));

    // A run whose summary carries this machine's cross-run reliability verdict.
    let mut summary = board_summary("aggregator", 0, Some("429 Too Many Requests"), None, None);
    summary.health = Some(crate::scraping::BoardHealth {
        status: crate::scraping::BoardHealthStatus::Failing,
        consecutive_failures: 4,
        last_success_at: Some(1_767_225_600_000),
        last_verified_at: Some(1_767_225_600_000),
        failing_since: Some(1_767_225_600_000),
        last_error: Some("429 Too Many Requests".into()),
        last_run_id: Some("job-secret".into()),
        verified_runs: 9,
        failed_runs: 4,
    });
    store.record_run(
        &ap.id,
        0,
        0,
        Vec::new(),
        vec![summary],
        &no_tombstones(),
        &[],
    );

    // Read the RAW on-disk bytes directly — not through any store's `get()`.
    // `AutopilotStore::load` ALSO scrubs `health` on every read now (closing
    // the same leak's third sink, an on-disk file from an intermediate
    // build), so reading back through a store — even a freshly reopened one
    // with a cold cache — can no longer prove `record_run` itself did the
    // stripping: `load`'s scrub would mask a `record_run` regression too.
    // Only the literal bytes `write_to_disk` produced prove THAT.
    let on_disk = std::fs::read_to_string(temp.path().join("autopilots.json")).unwrap();
    // The run's own diagnostics survive untouched…
    assert!(
        on_disk.contains("429 Too Many Requests"),
        "the run's own diagnostics must survive on disk; got: {on_disk}"
    );
    // …but the health verdict is a display-time derivation of the LIVE store and
    // must not be frozen into a persisted record.
    assert!(
        !on_disk.contains("\"health\""),
        "record_run must not persist the board-health verdict; got: {on_disk}"
    );
    assert!(
        !on_disk.contains("job-secret"),
        "record_run must not persist the board-health run id; got: {on_disk}"
    );

    // The load-bearing consequence: `AutopilotStore::export` writes
    // `lastRunSummaries` verbatim into the backup bundle, so a leak here would
    // replay THIS machine's failure streaks (and `lastRunId`) on another one.
    let bundle = {
        use crate::data_store::DataStore as _;
        serde_json::to_string(&store.export()).unwrap()
    };
    assert!(
        !bundle.contains("\"health\""),
        "the backup bundle must carry no board-health verdict"
    );
    assert!(
        !bundle.contains("job-secret"),
        "the backup bundle must carry no board-health run id"
    );
}

#[test]
fn export_strips_board_health_even_from_a_record_that_already_had_it() {
    use tempfile::TempDir;

    // Independent of `record_run`'s strip: a record written by an intermediate
    // build (or restored from one) can already carry a verdict, and `export` is
    // the boundary where it would actually leave the machine.
    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    store.create(serde_json::json!({
        "name": "ap",
        "target": { "board": "aggregator", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));

    let mut planted = board_summary("wwr", 0, Some("HTTP 500"), None, None);
    planted.health = Some(crate::scraping::BoardHealth {
        status: crate::scraping::BoardHealthStatus::Failing,
        consecutive_failures: 7,
        last_success_at: None,
        last_verified_at: Some(1_767_225_600_000),
        failing_since: Some(1_767_225_600_000),
        last_error: Some("machine-a-only".into()),
        last_run_id: Some("job-machine-a".into()),
        verified_runs: 11,
        failed_runs: 7,
    });
    let mut records = store.list();
    records[0].last_run_summaries = vec![planted];
    store.replace_all(records);

    let bundle = {
        use crate::data_store::DataStore as _;
        serde_json::to_string(&store.export()).unwrap()
    };
    // The run's own diagnostics still export — only the cross-run verdict is cut.
    assert!(
        bundle.contains("HTTP 500"),
        "the run summary itself exports"
    );
    for leak in [
        "\"health\"",
        "consecutiveFailures",
        "machine-a-only",
        "job-machine-a",
    ] {
        assert!(
            !bundle.contains(leak),
            "the bundle must not carry '{leak}'; got {bundle}"
        );
    }
}

#[test]
fn import_strips_board_health_from_a_legacy_or_tampered_bundle() {
    use tempfile::TempDir;

    // The other direction of `export_strips_board_health_even_from_a_record_
    // that_already_had_it`: a bundle that already carries `lastRunSummaries[].
    // health` (a pre-strip build's export, or a hand-edited backup) must not
    // land the verdict back on the machine that imports it.
    let source_dir = TempDir::new().unwrap();
    let source = AutopilotStore::new(&source_dir.path().to_path_buf());
    let ap = source.create(serde_json::json!({
        "name": "ap",
        "target": { "board": "aggregator", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));

    let mut planted = board_summary("wwr", 0, Some("HTTP 500"), None, None);
    planted.health = Some(crate::scraping::BoardHealth {
        status: crate::scraping::BoardHealthStatus::Failing,
        consecutive_failures: 7,
        last_success_at: None,
        last_verified_at: Some(1_767_225_600_000),
        failing_since: Some(1_767_225_600_000),
        last_error: Some("machine-a-only".into()),
        last_run_id: Some("job-machine-a".into()),
        verified_runs: 11,
        failed_runs: 7,
    });
    let mut records = source.list();
    records[0].last_run_summaries = vec![planted];
    let bundle = serde_json::to_value(&records).unwrap();
    // Sanity: the synthetic bundle really does carry the verdict pre-import.
    assert!(serde_json::to_string(&bundle)
        .unwrap()
        .contains("machine-a-only"));

    let restored_dir = TempDir::new().unwrap();
    let restored = AutopilotStore::new(&restored_dir.path().to_path_buf());
    {
        use crate::data_store::DataStore as _;
        restored.import(&bundle).unwrap();
    }

    // `restored`'s cache was just set directly by `import` → `replace_all` →
    // `save` — NOT via `load()` (`save` bypasses it) — so this proves
    // `import`'s OWN strip fired, independent of `load`'s separate scrub for
    // the same leak's third sink (see `load_strips_board_health_left_by_an_
    // intermediate_build`), which would otherwise mask a regression here.
    let reloaded = restored.get(&ap.id).unwrap();
    assert_eq!(reloaded.last_run_summaries.len(), 1);
    assert!(
        reloaded.last_run_summaries[0].health.is_none(),
        "import must strip the verdict; got {:?}",
        reloaded.last_run_summaries[0].health
    );

    // And the literal bytes `import`'s `save()` wrote to disk must not carry
    // it either — the strongest proof, independent of any store's read path.
    let on_disk = std::fs::read_to_string(restored_dir.path().join("autopilots.json")).unwrap();
    assert!(
        !on_disk.contains("\"health\"") && !on_disk.contains("machine-a-only"),
        "the verdict must not reach disk via import either; got: {on_disk}"
    );
}

#[test]
fn load_strips_board_health_left_by_an_intermediate_build() {
    use tempfile::TempDir;

    // The THIRD sink: an on-disk `autopilots.json` written before the
    // `record_run`/`export`/`import` strips existed (or hand-edited) can
    // still carry `lastRunSummaries[].health`. A cold `load()` (fresh store,
    // no warm cache) must scrub it going IN — otherwise any unrelated
    // mutation (`set_run_status`, `stamp_last_run`, …) would keep
    // re-persisting the stale verdict forever, since only `record_run`
    // itself ever touches that field.
    let temp = TempDir::new().unwrap();
    let dir = temp.path().to_path_buf();

    let seed = AutopilotStore::new(&dir);
    let ap = seed.create(serde_json::json!({
        "name": "ap",
        "target": { "board": "aggregator", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));
    let mut planted = board_summary("wwr", 0, Some("HTTP 500"), None, None);
    planted.health = Some(crate::scraping::BoardHealth {
        status: crate::scraping::BoardHealthStatus::Failing,
        consecutive_failures: 3,
        last_success_at: None,
        last_verified_at: Some(1_767_225_600_000),
        failing_since: Some(1_767_225_600_000),
        last_error: Some("stale-from-old-build".into()),
        last_run_id: Some("job-old".into()),
        verified_runs: 5,
        failed_runs: 3,
    });
    let mut records = seed.list();
    records[0].last_run_summaries = vec![planted];
    // Written directly — bypassing every strip this build has, simulating a
    // file this build never wrote through.
    std::fs::write(
        dir.join("autopilots.json"),
        serde_json::to_string_pretty(&records).unwrap(),
    )
    .unwrap();

    // Cold cache: this must go through `load()`'s parse path, not reuse
    // `seed`'s in-memory cache (which `save()` set directly, bypassing it).
    let cold = AutopilotStore::new(&dir);
    let reloaded = cold.get(&ap.id).unwrap();
    assert_eq!(reloaded.last_run_summaries.len(), 1);
    assert!(
        reloaded.last_run_summaries[0].health.is_none(),
        "a cold load must strip health left by an intermediate build; got {:?}",
        reloaded.last_run_summaries[0].health
    );
}

#[test]
fn fail_run_without_summaries_marks_failed_and_clears_stale_summaries() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "ap",
        "target": { "board": "aggregator", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));

    // Seed a prior SUCCESSFUL run — non-empty summaries + a Completed status —
    // the exact stale state `fail_run_without_summaries` must clear so a future
    // chip strip doesn't render the PRIOR run's per-board data as if it
    // belonged to the run that's about to fail outright.
    let summaries = vec![board_summary("greenhouse", 2, None, None, None)];
    store.record_run(&ap.id, 2, 0, Vec::new(), summaries, &no_tombstones(), &[]);
    let seeded = store.get(&ap.id).unwrap();
    assert_eq!(seeded.run_status, Some(RunStatus::Completed));
    assert_eq!(seeded.last_run_summaries.len(), 1);

    // An outright scrape error never reaches `record_run` (no fresh summaries
    // to report) — `fail_run_without_summaries` is the path taken instead.
    store.fail_run_without_summaries(&ap.id);

    let reloaded = store.get(&ap.id).unwrap();
    assert_eq!(
        reloaded.run_status,
        Some(RunStatus::Failed),
        "an outright scrape error must mark the run Failed"
    );
    assert!(
        reloaded.last_run_summaries.is_empty(),
        "the prior run's summaries must be cleared, not left stale on a Failed record"
    );
}

#[test]
fn fail_run_without_summaries_unknown_id_is_a_no_op() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());

    // Unknown id — must not panic (mirrors the "missing" no-op convention used
    // by `record_run`/`set_run_status` elsewhere in this file).
    store.fail_run_without_summaries("missing");
    assert!(
        store.list().is_empty(),
        "an unknown id must not create or otherwise mutate any record"
    );
}

#[test]
fn set_run_status_clearing_summaries_completed_clears_stale_summaries() {
    // Mirrors `fail_run_without_summaries_marks_failed_and_clears_stale_summaries`
    // for the OTHER caller of `set_run_status_clearing_summaries`: a user-cancelled
    // run, which sets `Completed` (not `Failed`) but likewise never reaches
    // `record_run`, so it must clear the PRIOR run's summaries too.
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "ap",
        "target": { "board": "aggregator", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));

    // Seed a prior successful run with real summaries — the stale state a
    // cancelled run must not inherit.
    let summaries = vec![board_summary("greenhouse", 2, None, None, None)];
    store.record_run(&ap.id, 2, 0, Vec::new(), summaries, &no_tombstones(), &[]);
    let seeded = store.get(&ap.id).unwrap();
    assert_eq!(seeded.run_status, Some(RunStatus::Completed));
    assert_eq!(seeded.last_run_summaries.len(), 1);

    // A cancelled run (never reaches `record_run`) sets Completed via the
    // clearing variant, same as the autopilot_run command's cancel branch.
    store.set_run_status_clearing_summaries(&ap.id, RunStatus::Completed);

    let reloaded = store.get(&ap.id).unwrap();
    assert_eq!(reloaded.run_status, Some(RunStatus::Completed));
    assert!(
        reloaded.last_run_summaries.is_empty(),
        "a cancelled run must clear the PRIOR run's summaries, not leave them stale"
    );
}

#[test]
fn autopilot_record_without_summaries_field_deserializes_to_empty() {
    // A record persisted before `lastRunSummaries` / the new `runStatus` variant
    // existed must still load — `#[serde(default)]` fills the missing field with
    // an empty list rather than failing the whole store read.
    let legacy = serde_json::json!({
        "_id": "legacy-1",
        "name": "Legacy",
        "status": "active",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
        "totalFound": 0,
        "totalApplied": 0,
        "createdAt": 1,
        "updatedAt": 1
        // no runStatus, no foundJobs, no lastRunSummaries
    });
    let ap: Autopilot = serde_json::from_value(legacy).expect("legacy record must deserialize");
    assert!(ap.last_run_summaries.is_empty());
    assert_eq!(ap.run_status, None);
}

#[test]
fn board_scrape_summary_legacy_single_note_field_deserializes_with_empty_notes() {
    // A record persisted by the old single-slot `note: Option<String>` shape
    // must still load. There is no `#[serde(alias = "note")]` — the legacy key
    // is silently ignored (unknown fields are dropped by default) and `notes`
    // takes its `#[serde(default)]` empty vec. Documented as an accepted,
    // already-established trade-off for this display-only field (see the doc
    // on `BoardScrapeSummary::notes`) — pinned here so it stays a DECISION,
    // not an unverified assumption.
    let legacy = serde_json::json!({
        "board": "greenhouse",
        "count": 6,
        "note": "location-filtered:5",
    });
    let back: crate::scraping::BoardScrapeSummary =
        serde_json::from_value(legacy).expect("legacy single-note record must deserialize");
    assert_eq!(back.board, "greenhouse");
    assert_eq!(back.count, 6);
    assert!(
        back.notes.is_empty(),
        "the legacy singular `note` key must NOT populate `notes` (no alias); got {:?}",
        back.notes
    );
}

#[test]
fn board_scrape_summary_round_trips_through_the_run_record() {
    // The run record persists `BoardScrapeSummary` (Serialize + Deserialize), so
    // a summary with every optional set must survive a serialize→deserialize
    // cycle unchanged — omitted `error`/`skipped` come back as `None`.
    let original = board_summary(
        "themuse",
        7,
        None,
        None,
        Some("page 3 of 5 failed: HTTP 429"),
    );
    let json = serde_json::to_string(&original).unwrap();
    let back: crate::scraping::BoardScrapeSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(back.board, "themuse");
    assert_eq!(back.count, 7);
    assert_eq!(back.error, None);
    assert_eq!(back.skipped, None);
    assert_eq!(
        back.truncated.as_deref(),
        Some("page 3 of 5 failed: HTTP 429")
    );
}

#[test]
fn test_data_store_export_import_preserves_id() {
    use crate::data_store::DataStore;
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let created = store.create(serde_json::json!({
        "name": "Test AP",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "manual",
    }));
    let id = created.id.clone();

    let bundle = store.export();

    let temp2 = TempDir::new().unwrap();
    let restored = AutopilotStore::new(&temp2.path().to_path_buf());
    let n = restored.import(&bundle).unwrap();

    assert_eq!(n, 1);
    let list = restored.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id); // id preserved across restore
    assert_eq!(list[0].name, "Test AP");
}

#[test]
fn save_skips_disk_write_when_serialized_state_is_unchanged() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    store.create(serde_json::json!({
        "name": "AP",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "manual",
    }));

    // After `create`, disk already holds exactly the serialized JSON, so the
    // dirty check compares equal and must skip the write. Probe with mtime: a
    // skipped write never touches the file (mtime frozen); a real rewrite would
    // bump it. Re-save the identical, unchanged map and assert mtime is stable.
    let file = temp.path().join("autopilots.json");
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();

    let map = store.load();
    store.save(map);

    let after = std::fs::metadata(&file).unwrap().modified().unwrap();
    assert_eq!(
        before, after,
        "identical serialized state must skip the write (mtime unchanged)"
    );
    // And the state is preserved, not blanked.
    assert!(std::fs::read_to_string(&file).unwrap().contains("\"_id\""));
}

#[test]
fn save_writes_when_serialized_state_differs() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    store.create(serde_json::json!({
        "name": "AP",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "manual",
    }));

    // Overwrite the file with content that does NOT match the serialized map,
    // then save the (unchanged) map: the bytes differ, so the write must proceed
    // and replace the sentinel content with the real serialized JSON.
    let file = temp.path().join("autopilots.json");
    std::fs::write(&file, "// stale sentinel content").unwrap();

    let map = store.load();
    store.save(map);

    let after = std::fs::read_to_string(&file).unwrap();
    assert!(
        !after.contains("stale sentinel"),
        "differing on-disk content must trigger a write"
    );
    assert!(
        after.contains("\"_id\""),
        "real serialized state was written"
    );
}

#[test]
fn save_writes_when_file_is_missing() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    store.create(serde_json::json!({
        "name": "AP",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "manual",
    }));

    // A missing/unreadable file never matches the serialized bytes → the write
    // must proceed so state isn't lost on the first persist after deletion.
    let file = temp.path().join("autopilots.json");
    std::fs::remove_file(&file).unwrap();
    assert!(!file.exists());

    let map = store.load();
    store.save(map);

    assert!(file.exists(), "missing file is (re)written, not skipped");
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(after.contains("\"_id\""), "serialized state was written");
}

#[test]
fn write_to_disk_surfaces_error_instead_of_swallowing_it() {
    use tempfile::TempDir;

    // Point the store's "data dir" at a real FILE, so `data_file` resolves to a
    // path *under* a non-directory. Neither the create_dir_all in `new` (`.ok`'d)
    // nor the final `std::fs::write` can create a child of a file, so the write
    // fails deterministically on every OS. This is exactly the error `save` now
    // logs via `log::error` instead of `.ok()`-swallowing (quick win 9) — proving
    // the error path is real and detectable rather than silently lost.
    let temp = TempDir::new().unwrap();
    let file_as_dir = temp.path().join("not-a-directory");
    std::fs::write(&file_as_dir, b"x").unwrap();

    let store = AutopilotStore::new(&file_as_dir);
    let result = store.write_to_disk(&HashMap::new());
    assert!(
        result.is_err(),
        "writing under a non-directory path must surface an IO error, not be swallowed"
    );

    // `save` (which now logs that error) must not panic on the failure path and
    // still keeps the in-memory cache consistent for the running process.
    store.save(HashMap::new());
    assert!(
        store.list().is_empty(),
        "save tolerates a persist failure without panicking and reflects the intended state in-memory"
    );
}

// ── AutopilotTarget boards back-compat deserialization ────────────────────────

#[test]
fn target_deserializes_legacy_board_string() {
    // Old on-disk format: `"board": "linkedin"` (singular string field).
    // The `#[serde(alias = "board", deserialize_with = "string_or_vec")]` must
    // normalise this to `boards: vec!["linkedin"]`.
    let json = r#"{"board": "linkedin", "query": "rust", "pages": 2}"#;
    let target: AutopilotTarget =
        serde_json::from_str(json).expect("legacy format must deserialize");
    assert_eq!(target.boards, vec!["linkedin"]);
}

#[test]
fn target_deserializes_new_boards_array() {
    // New format: `"boards": ["linkedin","remotive"]`.
    let json = r#"{"boards": ["linkedin","remotive"], "query": "rust", "pages": 2}"#;
    let target: AutopilotTarget = serde_json::from_str(json).expect("new format must deserialize");
    assert_eq!(target.boards, vec!["linkedin", "remotive"]);
}

#[test]
fn target_round_trips_as_boards_array() {
    // Serializing always writes `boards` (the canonical field name), so a
    // re-loaded record uses the new format — no legacy drift.
    let target = AutopilotTarget {
        boards: vec!["linkedin".to_string(), "remotive".to_string()],
        query: "rust".to_string(),
        location: None,
        country_code: None,
        work_types: None,
        pages: 2,
        date_filter: None,
        top_n: 3,
        watched_companies_only: None,
    };
    let serialized = serde_json::to_string(&target).unwrap();
    assert!(
        serialized.contains("\"boards\""),
        "must serialize as boards array"
    );
    assert!(
        !serialized.contains("\"board\""),
        "must not serialize as legacy singular"
    );

    let restored: AutopilotTarget = serde_json::from_str(&serialized).unwrap();
    assert_eq!(restored.boards, vec!["linkedin", "remotive"]);
}

// ── AutopilotTarget country_code serde ───────────────────────────────────────

#[test]
fn target_country_code_absent_deserializes_to_none() {
    // Backward-compat: a persisted autopilot that pre-dates the country_code field
    // (i.e. the JSON simply omits "countryCode") must still deserialize cleanly and
    // yield country_code: None. This guarantees old autopilots continue to load.
    let json = r#"{
        "boards": ["aggregator"],
        "query": "rust developer",
        "location": "London",
        "pages": 2,
        "topN": 3
    }"#;
    let target: AutopilotTarget =
        serde_json::from_str(json).expect("missing countryCode must not fail deserialization");
    assert!(
        target.country_code.is_none(),
        "absent countryCode field must deserialize to None"
    );
}

#[test]
fn target_country_code_round_trips_and_none_is_omitted() {
    // Round-trip: Some("us") survives serialize → deserialize.
    // Absence (None) must be omitted from JSON entirely (skip_serializing_if).
    let with_code = AutopilotTarget {
        boards: vec!["aggregator".to_string()],
        query: "frontend engineer".to_string(),
        location: None,
        country_code: Some("us".to_string()),
        work_types: None,
        pages: 1,
        date_filter: None,
        top_n: 3,
        watched_companies_only: None,
    };
    let json = serde_json::to_string(&with_code).unwrap();
    // camelCase rename_all means the field is "countryCode" on the wire.
    assert!(
        json.contains("\"countryCode\":\"us\""),
        "country_code Some(\"us\") must serialize as camelCase countryCode"
    );
    let restored: AutopilotTarget = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.country_code, Some("us".to_string()));

    // None must be omitted — not written as null or empty string.
    let without_code = AutopilotTarget {
        boards: vec!["aggregator".to_string()],
        query: "frontend engineer".to_string(),
        location: None,
        country_code: None,
        work_types: None,
        pages: 1,
        date_filter: None,
        top_n: 3,
        watched_companies_only: None,
    };
    let json_none = serde_json::to_string(&without_code).unwrap();
    assert!(
        !json_none.contains("countryCode"),
        "country_code None must be omitted from serialized JSON (skip_serializing_if)"
    );
}

// ── found_job helper ──────────────────────────────────────────────────────────

fn found_job(url: &str, found_at: u64) -> FoundJob {
    found_job_full(url, "Engineer", "Acme", found_at)
}

/// A [`FoundJob`] with an explicit title + company, so clustering-sensitive
/// tests can control whether two rows share a block (same title+company → one
/// cluster) or stay distinct.
fn found_job_full(url: &str, title: &str, company: &str, found_at: u64) -> FoundJob {
    FoundJob {
        title: title.into(),
        company: company.into(),
        url: url.into(),
        location: None,
        board: None,
        description: None,
        salary_min: None,
        salary_max: None,
        salary_currency: None,
        score: None,
        score_provisional: false,
        score_source: crate::autopilot::ScoreSource::Keyword,
        found_at,
        posted_at: None,
        is_new: false,
        applied: false,
        trust: None,
        assistant_notes: None,
        cluster_id: None,
        cluster_canonical: true,
        cluster_members: Vec::new(),
        is_agency: false,
    }
}

/// Empty tombstone set for record_run calls that don't exercise splits.
fn no_tombstones() -> std::collections::HashSet<(String, String)> {
    std::collections::HashSet::new()
}

// ── update_found_job_descriptions (issue #1106 part b) ────────────────────────

#[test]
fn update_found_job_descriptions_patches_the_matching_row() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "AP1",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));
    store.record_run(
        &ap.id,
        1,
        0,
        vec![found_job("https://boards.example.com/jobs/1", 1)],
        Vec::new(),
        &no_tombstones(),
        &[],
    );

    let normalized = crate::applications::normalize_job_url("https://boards.example.com/jobs/1");
    let updated = store.update_found_job_descriptions(&normalized, "corrected text");
    assert_eq!(updated, 1, "exactly one row must be patched");

    let list = store.list();
    assert_eq!(
        list[0].found_jobs[0].description.as_deref(),
        Some("corrected text")
    );
}

#[test]
fn update_found_job_descriptions_returns_zero_when_no_url_matches() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "AP1",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));
    store.record_run(
        &ap.id,
        1,
        0,
        vec![found_job("https://boards.example.com/jobs/1", 1)],
        Vec::new(),
        &no_tombstones(),
        &[],
    );

    let normalized = crate::applications::normalize_job_url("https://nowhere.example.com/x");
    let updated = store.update_found_job_descriptions(&normalized, "ignored");
    assert_eq!(updated, 0, "an unmatched url must patch nothing");
    assert_eq!(
        store.list()[0].found_jobs[0].description,
        None,
        "the unrelated row must be untouched on a miss"
    );
}

/// The same posting can legitimately surface under more than one autopilot
/// (two separate searches both matched it) — every row must update, not just
/// the first record iterated.
#[test]
fn update_found_job_descriptions_updates_every_matching_row_across_autopilots() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let shared_url = "https://boards.example.com/jobs/shared";

    let ap1 = store.create(serde_json::json!({
        "name": "AP1",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));
    let ap2 = store.create(serde_json::json!({
        "name": "AP2",
        "target": { "board": "indeed", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));
    store.record_run(
        &ap1.id,
        1,
        0,
        vec![found_job(shared_url, 1)],
        Vec::new(),
        &no_tombstones(),
        &[],
    );
    store.record_run(
        &ap2.id,
        1,
        0,
        vec![found_job(shared_url, 2)],
        Vec::new(),
        &no_tombstones(),
        &[],
    );

    let normalized = crate::applications::normalize_job_url(shared_url);
    let updated = store.update_found_job_descriptions(&normalized, "shared correction");
    assert_eq!(
        updated, 2,
        "both autopilots' rows for the same url must update, not just the first"
    );
    for ap in store.list() {
        assert_eq!(
            ap.found_jobs[0].description.as_deref(),
            Some("shared correction"),
            "every matching row across every autopilot must be patched"
        );
    }
}

// ── update_found_job_descriptions recomputes description-dependent trust but
// leaves score_provisional untouched (issue #1106 / #1106 shared-seam fix) ──
// `score_provisional` describes the `score` field, which a manual text
// correction deliberately does NOT recompute (see the doc comment on
// `update_found_job_descriptions`) — so the flag must survive unchanged here,
// only `trust` (genuinely description-scoped) may react.

#[test]
fn update_found_job_descriptions_clears_stale_trust_but_leaves_score_provisional_untouched() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "AP1",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));

    // `company_matches_host("Acme", "acme.com")` is true, so the ONLY trust
    // flag in play is the description-driven one — isolates the assertion
    // below to the thing this fix changes.
    let url = "https://acme.com/careers/1";
    let empty_desc_trust = crate::scraping::trust::assess_trust(url, "Acme", "");
    assert!(
        empty_desc_trust
            .flags
            .contains(&crate::scraping::trust::TrustFlag::DescriptionUnavailable),
        "seed sanity check: an empty description must carry DescriptionUnavailable"
    );

    let mut seeded = found_job_full(url, "Rust Engineer", "Acme", 1);
    seeded.score = Some(70.0);
    // Mirrors `build_found_job`'s `no_jd_text`-driven provisional marker for
    // a title-only posting (empty description, no requirements).
    seeded.score_provisional = true;
    seeded.trust = Some(empty_desc_trust);
    store.record_run(
        &ap.id,
        1,
        0,
        vec![seeded],
        Vec::new(),
        &no_tombstones(),
        &[],
    );

    let normalized = crate::applications::normalize_job_url(url);
    let full_description = "We are looking for a Senior Rust Engineer to build our \
         distributed systems platform. You will own the async runtime, mentor \
         junior engineers, and ship production services used by millions of \
         users daily.";
    let updated = store.update_found_job_descriptions(&normalized, full_description);
    assert_eq!(updated, 1);

    let job = &store.list()[0].found_jobs[0];
    let trust = job.trust.as_ref().expect("trust must still be set");
    assert!(
        !trust
            .flags
            .contains(&crate::scraping::trust::TrustFlag::DescriptionUnavailable),
        "a real, substantial description must clear the stale \
         DescriptionUnavailable flag, not keep showing a 'no description' \
         badge next to the now-visible full text"
    );
    assert_eq!(
        trust.level,
        crate::scraping::trust::TrustLevel::High,
        "clearing the only flag must read back as a higher trust level"
    );
    assert!(
        job.score_provisional,
        "score_provisional describes `score`, which a manual description \
         correction deliberately does not recompute — it must survive \
         unchanged (still true) until an actual autopilot run re-scores \
         the corrected content, never flip to false just because the \
         description changed"
    );
}

#[test]
fn merge_dedups_by_url_preserving_first_seen_and_flagging_new() {
    let existing = vec![found_job("https://a.com/1", 100)];
    let incoming = vec![
        found_job("https://a.com/1", 999), // re-surfaced — keep found_at=100
        found_job("https://a.com/2", 200), // genuinely new
    ];

    let merged = merge_found_jobs(&existing, incoming);

    assert_eq!(merged.len(), 2, "no duplicate row for the same url");
    let a1 = merged.iter().find(|j| j.url == "https://a.com/1").unwrap();
    assert_eq!(a1.found_at, 100, "first-seen time preserved");
    assert!(!a1.is_new, "an existing job is not new");
    let a2 = merged.iter().find(|j| j.url == "https://a.com/2").unwrap();
    assert!(a2.is_new, "a never-seen url is flagged new");
}

#[test]
fn merge_is_idempotent_on_a_repeated_run() {
    let first = merge_found_jobs(&[], vec![found_job("u1", 1), found_job("u2", 2)]);
    assert!(first.iter().all(|j| j.is_new));

    // Re-running with the same postings yields the same set; only is_new clears.
    let second = merge_found_jobs(&first, vec![found_job("u1", 9), found_job("u2", 9)]);
    assert_eq!(second.len(), 2);
    assert!(second.iter().all(|j| !j.is_new));
}

#[test]
fn merge_keeps_prior_jobs_not_in_the_new_run() {
    let existing = vec![found_job("old", 1)];
    let merged = merge_found_jobs(&existing, vec![found_job("fresh", 2)]);
    assert_eq!(merged.len(), 2, "prior finds retained below the new one");
    assert!(merged.iter().any(|j| j.url == "old"));
}

#[test]
fn merge_puts_newly_found_jobs_on_top() {
    let merged = merge_found_jobs(&[found_job("old", 1)], vec![found_job("fresh", 2)]);
    assert_eq!(
        merged[0].url, "fresh",
        "newly found job is first (top of list)"
    );
    assert!(merged[0].is_new);
    assert_eq!(merged[1].url, "old", "prior finds fall below the new one");
}

// ── Cross-source / canonical-URL dedup ────────────────────────────────────────

#[test]
fn merge_dedups_same_job_across_two_url_variants() {
    // Same job captured two ways: tracking query params vs. a hash fragment.
    let a = found_job(
        "https://boards.example.com/jobs/42?utm_source=aggregator",
        1,
    );
    let b = found_job("https://boards.example.com/jobs/42#apply", 2);

    let merged = merge_found_jobs(&[], vec![a, b]);

    assert_eq!(
        merged.len(),
        1,
        "tracking-param and hash variants of one job URL must merge to a single row"
    );
    assert!(merged[0].is_new, "the single merged row is newly surfaced");
}

#[test]
fn merge_collapses_persisted_row_against_a_new_url_variant() {
    // Back-compat: a found-job persisted under one raw URL must merge with an
    // incoming batch item that is a *variant* of the same URL (tracking params
    // vs. hash fragment) — they share one canonical key, so the re-surfaced job
    // updates the existing row instead of adding a duplicate, and only the truly
    // new job is flagged. Guards the merge_key → canonical_job_key delegation:
    // the algorithm is unchanged, so old-scheme keys still recompute identically.
    let persisted = found_job("https://boards.example.com/jobs/42?utm_source=x", 100);
    let variant = found_job("https://boards.example.com/jobs/42#apply", 999);
    let fresh = found_job("https://boards.example.com/jobs/99", 200);

    let merged = merge_found_jobs(&[persisted], vec![variant, fresh]);

    assert_eq!(
        merged.len(),
        2,
        "the variant merges into the persisted row; only the fresh job is added"
    );
    let resurfaced = merged
        .iter()
        .find(|j| j.url.contains("/jobs/42"))
        .expect("the persisted job's row survives");
    assert_eq!(resurfaced.found_at, 100, "first-seen time preserved");
    assert!(!resurfaced.is_new, "a re-surfaced URL variant is not new");
    let brand_new = merged
        .iter()
        .find(|j| j.url.contains("/jobs/99"))
        .expect("the fresh job is present");
    assert!(brand_new.is_new, "the never-seen job is flagged new");
}

#[test]
fn merge_dedups_internal_batch_duplicate() {
    // The same job surfaced by two sources (aggregator + a named board) in ONE run.
    let from_aggregator = found_job("https://jobs.example.com/eng-42", 1);
    let from_board = found_job("https://jobs.example.com/eng-42", 2);

    let merged = merge_found_jobs(&[], vec![from_aggregator, from_board]);

    assert_eq!(
        merged.len(),
        1,
        "an internal batch duplicate must produce exactly one row"
    );
}

#[test]
fn merge_distinct_jobs_are_unaffected_by_dedup() {
    let merged = merge_found_jobs(
        &[],
        vec![
            found_job("https://a.example.com/1", 1),
            found_job("https://b.example.com/2", 2),
            found_job("https://c.example.com/3", 3),
        ],
    );

    assert_eq!(
        merged.len(),
        3,
        "three distinct jobs must remain three rows"
    );
    assert!(merged.iter().all(|j| j.is_new));
}

#[test]
fn merge_within_batch_dup_keeps_longer_description() {
    // First-seen carries a short description; the later duplicate a longer one.
    let mut short = found_job("https://jobs.example.com/eng-42?ref=a", 1);
    short.description = Some("short".into());
    let mut long = found_job("https://jobs.example.com/eng-42?ref=b", 2);
    long.description = Some("a much longer and more complete description".into());

    let merged = merge_found_jobs(&[], vec![short, long]);

    assert_eq!(merged.len(), 1, "the two variants merge to one row");
    assert_eq!(
        merged[0].description.as_deref(),
        Some("a much longer and more complete description"),
        "the longer description from the later duplicate must win"
    );
}

#[test]
fn merge_dedups_url_less_jobs_by_title_and_company() {
    // No URL → fall back to a normalized title+company key. `found_job` sets
    // title "Engineer", company "Acme".
    let a = found_job("", 1); // url ""            → fallback key
    let b = found_job("   ", 2); // whitespace url  → normalizes to "" → same key
    let mut c = found_job("", 3);
    c.company = "Globex".into(); // same title, different company → distinct key

    let merged = merge_found_jobs(&[], vec![a, b, c]);

    assert_eq!(
        merged.len(),
        2,
        "URL-less jobs dedupe by normalized title+company: the two Acme rows merge, Globex stays"
    );
}

#[test]
fn record_run_new_count_reflects_deduped_batch() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "AP",
        "target": { "board": "aggregator", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 0.0 },
        "schedule": "manual",
    }));
    let id = ap.id;

    // One logical job surfaced by two sources (URL variants that canonicalize to
    // the same key) + one genuinely distinct job at a DIFFERENT company. The two
    // variants merge; the two distinct jobs sit in different clusters (distinct
    // companies → different blocks), so the cluster count is 2.
    let dup_a = found_job_full(
        "https://jobs.example.com/eng-1?utm_source=x",
        "Engineer",
        "AcmeOne",
        1,
    );
    let dup_b = found_job_full(
        "https://jobs.example.com/eng-1#frag",
        "Engineer",
        "AcmeOne",
        2,
    );
    let other = found_job_full("https://jobs.example.com/eng-2", "Engineer", "AcmeTwo", 3);

    let new_count = store.record_run(
        &id,
        3,
        0,
        vec![dup_a, dup_b, other],
        Vec::new(),
        &no_tombstones(),
        &[],
    );

    assert_eq!(
        new_count, 2,
        "the 'N new jobs' count must reflect the DEDUPED batch as CLUSTERS (2 distinct), not the raw 3"
    );
}

#[test]
fn record_run_reports_only_newly_surfaced_jobs() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let ap = store.create(serde_json::json!({
        "name": "AP",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": { "minMatchScore": 50.0 },
        "schedule": "manual",
    }));
    let id = ap.id;

    // Distinct companies keep each url in its own cluster, so the cluster count
    // tracks first-seen urls exactly (no accidental same-title merges).
    let j = |url: &str, company: &str, at: u64| found_job_full(url, "Engineer", company, at);

    // First run — both URLs are brand new → drives a "2 new jobs" notification.
    assert_eq!(
        store.record_run(
            &id,
            2,
            0,
            vec![j("u1", "Alpha", 1), j("u2", "Beta", 2)],
            Vec::new(),
            &no_tombstones(),
            &[],
        ),
        2
    );
    // Re-run with the two seen URLs + one unseen → only the unseen counts.
    assert_eq!(
        store.record_run(
            &id,
            3,
            0,
            vec![j("u1", "Alpha", 9), j("u2", "Beta", 9), j("u3", "Gamma", 9)],
            Vec::new(),
            &no_tombstones(),
            &[],
        ),
        1
    );
    // Nothing unseen → no notification.
    assert_eq!(
        store.record_run(
            &id,
            3,
            0,
            vec![j("u1", "Alpha", 9)],
            Vec::new(),
            &no_tombstones(),
            &[]
        ),
        0
    );
    // Unknown autopilot → 0 (no panic).
    assert_eq!(
        store.record_run(
            "missing",
            5,
            0,
            vec![j("x", "Alpha", 1)],
            Vec::new(),
            &no_tombstones(),
            &[]
        ),
        0
    );
}

// ── Cross-board cluster counts + split survival (ADR-029 §f/§h) ───────────────

/// One direct-board FoundJob with an explicit board id (source).
fn board_job(url: &str, board: &str, at: u64) -> FoundJob {
    FoundJob {
        board: Some(board.into()),
        ..found_job_full(url, "Rust Developer", "Acme", at)
    }
}

fn manual_ap(store: &AutopilotStore) -> String {
    store
        .create(serde_json::json!({
            "name": "AP",
            "target": { "board": "linkedin", "query": "rust", "pages": 1 },
            "filter": { "minMatchScore": 0.0 },
            "schedule": "manual",
        }))
        .id
}

#[test]
fn record_run_cluster_count_two_board_new_is_one() {
    use tempfile::TempDir;
    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let id = manual_ap(&store);

    // The SAME job on two boards (same title+company, distinct urls/sources) →
    // one cluster, all members new → the notification count is 1, not 2.
    let a = board_job("https://a.example.com/1", "greenhouse", 1);
    let b = board_job("https://b.example.com/2", "aggregator", 2);
    let new_count = store.record_run(&id, 2, 0, vec![a, b], Vec::new(), &no_tombstones(), &[]);
    assert_eq!(
        new_count, 1,
        "one job on two boards counts as ONE new cluster"
    );
}

#[test]
fn record_run_cluster_count_known_job_resurfacing_is_zero() {
    use tempfile::TempDir;
    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let id = manual_ap(&store);

    // Run 1: the job is first seen on a direct board → 1 new.
    let direct = board_job("https://a.example.com/1", "greenhouse", 1);
    assert_eq!(
        store.record_run(&id, 1, 0, vec![direct], Vec::new(), &no_tombstones(), &[]),
        1
    );

    // Run 2: the SAME job resurfaces via the aggregator (new url) — it clusters
    // with the known direct row, so the cluster is not all-new → 0.
    let agg = board_job("https://b.example.com/2", "aggregator", 2);
    assert_eq!(
        store.record_run(&id, 2, 0, vec![agg], Vec::new(), &no_tombstones(), &[]),
        0,
        "a known job resurfacing on another board contributes 0 new"
    );
}

#[test]
fn split_survives_two_record_run_cycles() {
    use tempfile::TempDir;
    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());
    let id = manual_ap(&store);

    let a = board_job("https://a.example.com/1", "greenhouse", 1);
    let b = board_job("https://b.example.com/2", "lever", 2);

    // Without a tombstone the two cluster together.
    store.record_run(
        &id,
        2,
        0,
        vec![a.clone(), b.clone()],
        Vec::new(),
        &no_tombstones(),
        &[],
    );
    let jobs = store.get(&id).unwrap().found_jobs;
    let cid_a = jobs
        .iter()
        .find(|j| j.url.contains("a.example"))
        .and_then(|j| j.cluster_id.clone());
    let cid_b = jobs
        .iter()
        .find(|j| j.url.contains("b.example"))
        .and_then(|j| j.cluster_id.clone());
    assert_eq!(cid_a, cid_b, "no tombstone → same cluster");

    // Tombstone their canonical keys, then re-run TWICE — the split must hold.
    let key_a = crate::scraping::boards::common::canonical_job_key(
        "https://a.example.com/1",
        "Rust Developer",
        "Acme",
    );
    let key_b = crate::scraping::boards::common::canonical_job_key(
        "https://b.example.com/2",
        "Rust Developer",
        "Acme",
    );
    let mut tombstones = std::collections::HashSet::new();
    tombstones.insert(crate::dedup::DedupStore::pair(&key_a, &key_b));

    for cycle in 0..2 {
        store.record_run(
            &id,
            2,
            0,
            vec![a.clone(), b.clone()],
            Vec::new(),
            &tombstones,
            &[],
        );
        let jobs = store.get(&id).unwrap().found_jobs;
        let ca = jobs
            .iter()
            .find(|j| j.url.contains("a.example"))
            .and_then(|j| j.cluster_id.clone());
        let cb = jobs
            .iter()
            .find(|j| j.url.contains("b.example"))
            .and_then(|j| j.cluster_id.clone());
        assert_ne!(ca, cb, "tombstone split must survive cycle {cycle}");
    }
}

#[test]
fn found_job_without_board_deserializes_to_none() {
    // Old persisted FoundJob records pre-date the `board` field. The
    // `#[serde(default)]` must let them load with `board: None` rather than failing.
    let json = r#"{
        "title": "Engineer",
        "company": "Acme",
        "url": "https://a.com/1",
        "foundAt": 100
    }"#;
    let job: FoundJob = serde_json::from_str(json).expect("legacy FoundJob must deserialize");
    assert_eq!(job.board, None, "absent board must default to None");
}

#[test]
fn found_job_with_board_round_trips() {
    // A FoundJob carrying a board serializes the camelCase key and round-trips.
    let mut job = found_job("https://a.com/1", 100);
    job.board = Some("aggregator".into());
    let json = serde_json::to_string(&job).unwrap();
    assert!(
        json.contains("\"board\":\"aggregator\""),
        "board must serialize as a camelCase string; got {json}"
    );
    let restored: FoundJob = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.board, Some("aggregator".to_string()));
}

#[test]
fn legacy_found_job_without_posted_at_deserializes_to_none() {
    // Old persisted FoundJob records pre-date the `postedAt` field. The
    // `#[serde(default)]` must let them load with `posted_at: None` rather than
    // failing, and re-serialize without emitting a `postedAt` key at all
    // (`skip_serializing_if`) — a legacy record must load AND save unchanged.
    let json = r#"{
        "title": "Engineer",
        "company": "Acme",
        "url": "https://a.com/1",
        "foundAt": 100
    }"#;
    let job: FoundJob = serde_json::from_str(json).expect("legacy FoundJob must deserialize");
    assert_eq!(job.posted_at, None, "absent postedAt must default to None");

    let round_tripped = serde_json::to_string(&job).unwrap();
    assert!(
        !round_tripped.contains("postedAt"),
        "a legacy record with no posted_at must not grow a postedAt key on save; got {round_tripped}"
    );
    let restored: FoundJob =
        serde_json::from_str(&round_tripped).expect("re-serialized legacy record must load");
    assert_eq!(restored.posted_at, None);
}

#[test]
fn found_job_with_posted_at_round_trips() {
    // A FoundJob carrying the posting's publish date serializes the camelCase
    // key as epoch millis and round-trips.
    let mut job = found_job("https://a.com/1", 100);
    job.posted_at = Some(1_700_000_000_000);
    let json = serde_json::to_string(&job).unwrap();
    assert!(
        json.contains("\"postedAt\":1700000000000"),
        "postedAt must serialize as a camelCase epoch-ms number; got {json}"
    );
    let restored: FoundJob = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.posted_at, Some(1_700_000_000_000));
}

#[test]
fn merge_preserves_and_refreshes_board_across_resurface() {
    // An existing row persisted before `board` existed (None) must pick up the
    // board when the same URL re-surfaces, and a never-seen URL keeps its board.
    let mut existing = found_job("https://a.com/1", 100);
    existing.board = None; // legacy row, no provenance yet

    let mut resurfaced = found_job("https://a.com/1", 999);
    resurfaced.board = Some("linkedin".into());
    let mut fresh = found_job("https://a.com/2", 200);
    fresh.board = Some("aggregator".into());

    let merged = merge_found_jobs(&[existing], vec![resurfaced, fresh]);

    let a1 = merged.iter().find(|j| j.url == "https://a.com/1").unwrap();
    assert_eq!(
        a1.board,
        Some("linkedin".to_string()),
        "re-surfaced existing row picks up the incoming board"
    );
    let a2 = merged.iter().find(|j| j.url == "https://a.com/2").unwrap();
    assert_eq!(
        a2.board,
        Some("aggregator".to_string()),
        "appended new row keeps its board (via ..inc spread)"
    );
}

#[test]
fn merge_preserves_a_real_description_over_a_blank_resurface() {
    // LinkedIn search results always carry `description: Some("")` (never
    // `None`) — that is its "unknown" sentinel, not `None`. A posting
    // enriched by `autopilot_helpers::linkedin_enrich` on a prior run must
    // NOT lose that real description just because it resurfaces in a fresh
    // LinkedIn scrape, which reports the same blank sentinel every time the
    // posting is still listed.
    let mut existing = found_job("https://a.com/1", 100);
    existing.description = Some("A real, fetched job description.".to_string());

    let mut resurfaced = found_job("https://a.com/1", 999);
    resurfaced.description = Some(String::new()); // LinkedIn's blank sentinel

    let merged = merge_found_jobs(&[existing], vec![resurfaced]);

    let a1 = merged.iter().find(|j| j.url == "https://a.com/1").unwrap();
    assert_eq!(
        a1.description,
        Some("A real, fetched job description.".to_string()),
        "a blank resurface must never clobber an already-known real description"
    );
}

#[test]
fn merge_preserves_and_refreshes_trust_across_resurface() {
    // Same legacy-migration case as the board test above: an existing row
    // persisted before `trust` existed (None) must pick up the incoming trust
    // when the same URL re-surfaces, and a never-seen URL keeps its trust.
    let mut existing = found_job("https://a.com/1", 100);
    existing.trust = None; // legacy row, no trust assessment yet

    let resurfaced_trust = crate::scraping::trust::assess_trust(
        "https://linkedin.com/jobs/view/1",
        "Acme",
        "A real description.",
    );
    let mut resurfaced = found_job("https://a.com/1", 999);
    resurfaced.trust = Some(resurfaced_trust.clone());
    let fresh_trust = crate::scraping::trust::assess_trust(
        "https://boards.greenhouse.io/acme/jobs/2",
        "Acme",
        "A real description.",
    );
    let mut fresh = found_job("https://a.com/2", 200);
    fresh.trust = Some(fresh_trust.clone());

    let merged = merge_found_jobs(&[existing], vec![resurfaced, fresh]);

    let a1 = merged.iter().find(|j| j.url == "https://a.com/1").unwrap();
    assert_eq!(
        a1.trust,
        Some(resurfaced_trust),
        "re-surfaced existing row picks up the incoming trust"
    );
    let a2 = merged.iter().find(|j| j.url == "https://a.com/2").unwrap();
    assert_eq!(
        a2.trust,
        Some(fresh_trust),
        "appended new row keeps its trust (via ..inc spread)"
    );
}

#[test]
fn merge_preserves_and_refreshes_posted_at_across_resurface() {
    // Same legacy-migration case as the board/trust tests above: an existing
    // row persisted before `posted_at` existed (None) — or scraped from a
    // board that didn't expose a publish date on an earlier run — must pick up
    // the incoming date when the same URL re-surfaces, and a never-seen URL
    // keeps its own date.
    let mut existing = found_job("https://a.com/1", 100);
    existing.posted_at = None; // legacy/dateless row

    let mut resurfaced = found_job("https://a.com/1", 999);
    resurfaced.posted_at = Some(1_700_000_000_000);
    let mut fresh = found_job("https://a.com/2", 200);
    fresh.posted_at = Some(1_650_000_000_000);

    let merged = merge_found_jobs(&[existing], vec![resurfaced, fresh]);

    let a1 = merged.iter().find(|j| j.url == "https://a.com/1").unwrap();
    assert_eq!(
        a1.posted_at,
        Some(1_700_000_000_000),
        "re-surfaced existing row backfills the incoming posted_at"
    );
    let a2 = merged.iter().find(|j| j.url == "https://a.com/2").unwrap();
    assert_eq!(
        a2.posted_at,
        Some(1_650_000_000_000),
        "appended new row keeps its posted_at (via ..inc spread)"
    );
}

#[test]
fn merge_keeps_a_known_posted_at_when_the_resurfaced_row_has_none() {
    // The mirror of the backfill case above: a row with an already-known date
    // must NOT lose it when the same job re-surfaces from a board/run that
    // didn't report one this time (several boards send no date at all, and
    // even a board that usually does can drop it on one page). The guard is
    // `if inc.posted_at.is_some()` — an unconditional `row.posted_at =
    // inc.posted_at` would silently erase a known date here.
    let mut existing = found_job("https://a.com/1", 100);
    existing.posted_at = Some(1_700_000_000_000); // known date from a prior run

    let mut resurfaced = found_job("https://a.com/1", 999);
    resurfaced.posted_at = None; // this run's copy has no date

    let merged = merge_found_jobs(&[existing], vec![resurfaced]);

    let a1 = merged.iter().find(|j| j.url == "https://a.com/1").unwrap();
    assert_eq!(
        a1.posted_at,
        Some(1_700_000_000_000),
        "a known posted_at must survive a resurface that reports no date"
    );
}

#[test]
fn merge_score_provisional_moves_with_score_across_resurface() {
    // `score_provisional` describes WHICH score is on the row, so a resurface
    // that refreshes `score` must refresh the flag alongside it — in BOTH
    // directions. Resurfacing is ORDINARY autopilot behavior (the same job
    // returned by more than one board, or seen again on a later run), not an
    // edge case, so a desync here would be routinely user-visible.

    // (a) aggregator-first (provisional score) resurfaced by a full-text board
    // (authoritative score) → the flag must flip to false with the new score.
    let mut existing_provisional = found_job("https://a.com/1", 100);
    existing_provisional.score = Some(40.0);
    existing_provisional.score_provisional = true;

    let mut resurfaced_authoritative = found_job("https://a.com/1", 999);
    resurfaced_authoritative.score = Some(72.0);
    resurfaced_authoritative.score_provisional = false;

    let merged = merge_found_jobs(&[existing_provisional], vec![resurfaced_authoritative]);
    let row = merged.iter().find(|j| j.url == "https://a.com/1").unwrap();
    assert_eq!(row.score, Some(72.0));
    assert!(
        !row.score_provisional,
        "a full-text board's authoritative score resurfacing over an old \
         aggregator snippet score must clear the provisional flag"
    );

    // (b) full-text-first (authoritative) resurfaced by the aggregator
    // (snippet) → the flag must flip to TRUE with the snippet score — the
    // worse direction: a snippet score must never display as authoritative.
    let mut existing_authoritative = found_job("https://b.com/1", 100);
    existing_authoritative.score = Some(72.0);
    existing_authoritative.score_provisional = false;

    let mut resurfaced_provisional = found_job("https://b.com/1", 999);
    resurfaced_provisional.score = Some(40.0);
    resurfaced_provisional.score_provisional = true;

    let merged = merge_found_jobs(&[existing_authoritative], vec![resurfaced_provisional]);
    let row = merged.iter().find(|j| j.url == "https://b.com/1").unwrap();
    assert_eq!(row.score, Some(40.0));
    assert!(
        row.score_provisional,
        "an aggregator snippet score resurfacing over a prior authoritative \
         score must set the provisional flag — a snippet score must never \
         display as authoritative"
    );
}

#[test]
fn merge_score_source_moves_with_score_across_resurface() {
    // The mirror of the test above, for the OTHER field paired with `score`.
    // `score_source` says which KERNEL produced the number, and it drives the
    // user-facing label (`autopilot.scoreLabel.{coverage,combined}`) plus its
    // tier cut points — so a resurface that refreshes `score` must refresh
    // `score_source` alongside it, in both directions.

    // (a) a semantic re-rank's combined score, resurfaced by a later run whose
    // re-rank never reached this job (semantic turned off, the daily ceiling,
    // the wall clock, the degrade breaker, an offline provider). The keyword
    // number must not inherit the previous run's "combined" label.
    let mut existing_combined = found_job("https://a.com/1", 100);
    existing_combined.score = Some(91.0);
    existing_combined.score_source = ScoreSource::Combined;

    let mut resurfaced_keyword = found_job("https://a.com/1", 999);
    resurfaced_keyword.score = Some(62.0);
    resurfaced_keyword.score_source = ScoreSource::Keyword;

    let merged = merge_found_jobs(&[existing_combined], vec![resurfaced_keyword]);
    let row = merged.iter().find(|j| j.url == "https://a.com/1").unwrap();
    assert_eq!(row.score, Some(62.0));
    assert_eq!(
        row.score_source,
        ScoreSource::Keyword,
        "a keyword score resurfacing over a prior combined score must carry the \
         keyword label — the two kernels are different scales, and the stale \
         label would relabel 62 as a semantic verdict it never was"
    );

    // (b) the reverse: a keyword row that this run's re-rank DID reach must
    // pick the combined label up, or the semantic number keeps being displayed
    // (and banded) as plain coverage.
    let mut existing_keyword = found_job("https://b.com/1", 100);
    existing_keyword.score = Some(62.0);
    existing_keyword.score_source = ScoreSource::Keyword;

    let mut resurfaced_combined = found_job("https://b.com/1", 999);
    resurfaced_combined.score = Some(91.0);
    resurfaced_combined.score_source = ScoreSource::Combined;

    let merged = merge_found_jobs(&[existing_keyword], vec![resurfaced_combined]);
    let row = merged.iter().find(|j| j.url == "https://b.com/1").unwrap();
    assert_eq!(row.score, Some(91.0));
    assert_eq!(
        row.score_source,
        ScoreSource::Combined,
        "a re-ranked score must arrive with its own label"
    );
}

// ── AutopilotStore::create filter fallback ────────────────────────────────────

#[test]
fn create_with_missing_filter_defaults_min_match_score_to_zero() {
    use tempfile::TempDir;

    // When `filter` is absent (or null) the store must default min_match_score
    // to 0.0 — NOT 50.0. A 50.0 default silently drops most scraped jobs.
    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());

    let ap = store.create(serde_json::json!({
        "name": "No filter",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        // `filter` key completely omitted
        "schedule": "daily",
    }));
    assert_eq!(
        ap.filter.min_match_score, 0.0,
        "absent filter must default to min_match_score 0.0, not 50.0"
    );
    assert!(ap.filter.keywords.is_none());
    assert!(ap.filter.exclude_keywords.is_none());
}

#[test]
fn create_with_null_filter_defaults_min_match_score_to_zero() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store = AutopilotStore::new(&temp.path().to_path_buf());

    let ap = store.create(serde_json::json!({
        "name": "Null filter",
        "target": { "board": "linkedin", "query": "rust", "pages": 1 },
        "filter": null,
        "schedule": "daily",
    }));
    assert_eq!(
        ap.filter.min_match_score, 0.0,
        "null filter must default to min_match_score 0.0"
    );
}

// ── relax_legacy_filters ──────────────────────────────────────────────────────

/// Return a fully-populated `Autopilot` that each test can mutate in place.
/// Starts with the legacy restrictive defaults (the zero-jobs configuration)
/// so most tests only need to tweak the one field they care about.
/// Zero args → well under the 8-arg clippy limit.
fn base_autopilot() -> Autopilot {
    let now = 1_000_000u64;
    Autopilot {
        id: "test-id".into(),
        name: "Test AP".into(),
        status: AutopilotStatus::Active,
        target: AutopilotTarget {
            boards: vec!["linkedin".into()],
            query: "engineer".into(),
            location: None,
            country_code: None,
            work_types: None,
            pages: 1,
            date_filter: Some("24h".into()),
            top_n: 3,
            watched_companies_only: None,
        },
        filter: AutopilotFilter {
            min_match_score: 50.0,
            keywords: Some(vec!["rust".into(), "go".into()]),
            exclude_keywords: None,
        },
        schedule: "daily".into(),
        schedule_hour: None,
        schedule_minute: None,
        resume_text: None,
        cover_letter: None,
        assistant: false,
        assistant_provider: None,
        assistant_model: None,
        assistant_base_url: None,
        total_found: 0,
        total_applied: 0,
        found_jobs: Vec::new(),
        run_status: None,
        last_run_summaries: Vec::new(),
        last_run_at: None,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn relax_clears_keywords_for_legacy_record() {
    // base_autopilot() is legacy (score 50 + date "24h"), so the auto-prefilled
    // keyword list is cleared. The clear is gated on legacy-ness (see
    // `relax_is_noop_on_already_relaxed_record` for the non-legacy path).
    let mut ap = base_autopilot();
    ap.filter.keywords = Some(vec!["rust".into(), "go".into()]);
    relax_legacy_filters(&mut ap);
    assert!(
        ap.filter.keywords.is_none(),
        "legacy record's prefilled keywords must be cleared to None"
    );
}

#[test]
fn relax_is_noop_on_already_relaxed_record() {
    // A record already relaxed (score 0.0, date None) is NOT legacy, so re-running
    // the migration must NOT touch keywords the user added afterwards. This is the
    // idempotency property that makes a marker-write failure (→ rerun) safe.
    let mut ap = base_autopilot();
    ap.filter.min_match_score = 0.0;
    ap.target.date_filter = None;
    ap.filter.keywords = Some(vec!["python".into()]);

    relax_legacy_filters(&mut ap);

    assert_eq!(
        ap.filter.keywords.as_deref(),
        Some(["python".to_string()].as_ref()),
        "user-added keywords on an already-relaxed record must survive a rerun"
    );
    assert_eq!(ap.filter.min_match_score, 0.0);
    assert!(ap.target.date_filter.is_none());
}

#[test]
fn relax_keeps_keywords_when_not_legacy() {
    // Pins the documented narrow gap (autopilot/mod.rs `was_legacy`): a record
    // with prefilled keywords where the user ALSO changed BOTH the score (≠50.0)
    // AND the date (≠"24h") reads as non-legacy, so its keywords are KEPT. This
    // guards against a future change to the `was_legacy` predicate silently
    // regressing the "err toward keeping user data" direction.
    let mut ap = base_autopilot();
    ap.filter.min_match_score = 30.0; // ≠ 50.0
    ap.target.date_filter = Some("week".into()); // ≠ "24h"
    ap.filter.keywords = Some(vec!["python".into()]);

    relax_legacy_filters(&mut ap);

    assert_eq!(
        ap.filter.keywords.as_deref(),
        Some(["python".to_string()].as_ref()),
        "non-legacy record (score≠50 AND date≠24h) must keep its keywords"
    );
    assert_eq!(
        ap.filter.min_match_score, 30.0,
        "non-default score must be left untouched"
    );
    assert_eq!(
        ap.target.date_filter.as_deref(),
        Some("week"),
        "non-default date_filter must be left untouched"
    );
}

#[test]
fn relax_clears_none_keywords_remains_none() {
    // keywords already None → still None (no-op, no panic).
    let mut ap = base_autopilot();
    ap.filter.keywords = None;
    relax_legacy_filters(&mut ap);
    assert!(ap.filter.keywords.is_none());
}

#[test]
fn relax_resets_min_match_score_only_when_exactly_50() {
    // 50.0 → reset to 0.0.
    let mut ap = base_autopilot();
    ap.filter.min_match_score = 50.0;
    relax_legacy_filters(&mut ap);
    assert_eq!(
        ap.filter.min_match_score, 0.0,
        "default 50.0 must be reset to 0.0"
    );
}

#[test]
fn relax_leaves_custom_min_match_score_untouched() {
    // 75.0 (deliberate user setting) → unchanged.
    let mut ap = base_autopilot();
    ap.filter.min_match_score = 75.0;
    relax_legacy_filters(&mut ap);
    assert_eq!(
        ap.filter.min_match_score, 75.0,
        "custom 75.0 must not be touched"
    );
}

#[test]
fn relax_leaves_already_zero_min_match_score_at_zero() {
    // Already 0.0 → stays 0.0 (idempotent / already relaxed).
    let mut ap = base_autopilot();
    ap.filter.min_match_score = 0.0;
    relax_legacy_filters(&mut ap);
    assert_eq!(ap.filter.min_match_score, 0.0);
}

#[test]
fn relax_leaves_near_fifty_min_match_score_untouched() {
    // 49.9 is close to but NOT the magic 50.0 → unchanged.
    let mut ap = base_autopilot();
    ap.filter.min_match_score = 49.9;
    relax_legacy_filters(&mut ap);
    assert_eq!(
        ap.filter.min_match_score, 49.9,
        "49.9 is not the legacy default; must be left unchanged"
    );
}

#[test]
fn relax_clears_date_filter_only_for_24h() {
    // "24h" is the legacy auto-default → should become None.
    let mut ap = base_autopilot();
    ap.target.date_filter = Some("24h".into());
    relax_legacy_filters(&mut ap);
    assert!(
        ap.target.date_filter.is_none(),
        "\"24h\" legacy default must be cleared to None"
    );
}

#[test]
fn relax_leaves_week_date_filter_untouched() {
    let mut ap = base_autopilot();
    ap.target.date_filter = Some("week".into());
    relax_legacy_filters(&mut ap);
    assert_eq!(
        ap.target.date_filter.as_deref(),
        Some("week"),
        "user-picked \"week\" must be left alone"
    );
}

#[test]
fn relax_leaves_month_date_filter_untouched() {
    let mut ap = base_autopilot();
    ap.target.date_filter = Some("month".into());
    relax_legacy_filters(&mut ap);
    assert_eq!(
        ap.target.date_filter.as_deref(),
        Some("month"),
        "user-picked \"month\" must be left alone"
    );
}

#[test]
fn relax_leaves_none_date_filter_as_none() {
    let mut ap = base_autopilot();
    ap.target.date_filter = None;
    relax_legacy_filters(&mut ap);
    assert!(ap.target.date_filter.is_none());
}

#[test]
fn relax_preserves_all_unrelated_fields() {
    let mut ap = base_autopilot();
    // Set the fields relax touches (legacy defaults).
    ap.filter.keywords = Some(vec!["rust".into()]);
    ap.filter.min_match_score = 50.0;
    ap.target.date_filter = Some("24h".into());
    // Set non-relax fields to non-default values so we can assert they survive.
    ap.filter.exclude_keywords = Some(vec!["senior".into()]);
    ap.target.query = "backend engineer".into();
    ap.target.location = Some("Berlin".into());
    ap.target.country_code = Some("de".into());
    ap.target.boards = vec!["linkedin".into(), "indeed".into()];
    ap.target.pages = 3;
    ap.target.work_types = Some(vec![WorkType::Remote]);

    relax_legacy_filters(&mut ap);

    // The fix clears keywords + resets score + clears date_filter.
    assert!(ap.filter.keywords.is_none());
    assert_eq!(ap.filter.min_match_score, 0.0);
    assert!(ap.target.date_filter.is_none());

    // Everything else must be untouched.
    assert_eq!(
        ap.filter.exclude_keywords.as_deref(),
        Some(["senior".to_string()].as_ref()),
        "exclude_keywords must be preserved"
    );
    assert_eq!(ap.target.query, "backend engineer");
    assert_eq!(ap.target.location.as_deref(), Some("Berlin"));
    assert_eq!(ap.target.country_code.as_deref(), Some("de"));
    assert_eq!(ap.target.boards, vec!["linkedin", "indeed"]);
    assert_eq!(ap.target.pages, 3);
    assert_eq!(ap.target.work_types, Some(vec![WorkType::Remote]));
}

#[test]
fn relax_is_idempotent() {
    // Calling relax_legacy_filters twice must equal calling it once — the
    // second call is a no-op on an already-relaxed autopilot.
    let mut ap = base_autopilot();
    // Start from the worst-case legacy state.
    ap.filter.keywords = Some(vec!["rust".into()]);
    ap.filter.exclude_keywords = Some(vec!["senior".into()]);
    ap.filter.min_match_score = 50.0;
    ap.target.date_filter = Some("24h".into());

    relax_legacy_filters(&mut ap);
    let after_first = (
        ap.filter.keywords.clone(),
        ap.filter.min_match_score,
        ap.target.date_filter.clone(),
    );

    relax_legacy_filters(&mut ap);
    let after_second = (
        ap.filter.keywords.clone(),
        ap.filter.min_match_score,
        ap.target.date_filter.clone(),
    );

    assert_eq!(after_first, after_second, "second call must be a no-op");
}

// ── relax_legacy_filters_once (I/O orchestration) ────────────────────────────

/// Seed a store with one restrictive autopilot (the legacy defaults that caused
/// zero-jobs) and return its id. Shared setup for the `_once` tests.
fn seed_restrictive(store: &AutopilotStore) -> String {
    store
        .create(serde_json::json!({
            "name": "Legacy",
            "target": {
                "board": "linkedin",
                "query": "rust",
                "pages": 1,
                "dateFilter": "24h"
            },
            "filter": {
                "minMatchScore": 50.0,
                "keywords": ["rust", "go"]
            },
            "schedule": "daily",
        }))
        .id
}

#[test]
fn relax_legacy_filters_once_relaxes_and_writes_marker_on_first_run() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let dir = temp.path().to_path_buf();
    let store = AutopilotStore::new(&dir);
    let id = seed_restrictive(&store);

    // Marker must not exist before the first run.
    let marker = dir.join(RELAX_MARKER_FILE);
    assert!(!marker.exists(), "marker must be absent before migration");

    store.relax_legacy_filters_once();

    // (a) Marker written after a successful first run.
    assert!(marker.exists(), "marker must be created after first run");

    // (b) On-disk autopilot has been relaxed.
    let ap = store.get(&id).expect("autopilot must still exist");
    assert_eq!(
        ap.filter.min_match_score, 0.0,
        "min_match_score must be reset from 50.0 to 0.0"
    );
    assert!(
        ap.filter.keywords.is_none(),
        "keywords must be cleared to None"
    );
    assert!(
        ap.target.date_filter.is_none(),
        "date_filter must be cleared from \"24h\" to None"
    );
}

#[test]
fn relax_legacy_filters_once_skips_when_marker_present() {
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let dir = temp.path().to_path_buf();
    let store = AutopilotStore::new(&dir);
    let id = seed_restrictive(&store);

    // Pre-create the marker — simulates a store that was already migrated.
    let marker = dir.join(RELAX_MARKER_FILE);
    std::fs::write(&marker, b"1").unwrap();

    store.relax_legacy_filters_once();

    // The autopilot must be completely unchanged (still restrictive).
    let ap = store.get(&id).expect("autopilot must still exist");
    assert_eq!(
        ap.filter.min_match_score, 50.0,
        "min_match_score must be left at 50.0 when marker is present"
    );
    assert!(
        ap.filter.keywords.is_some(),
        "keywords must remain Some([...]) when marker is present"
    );
    assert_eq!(
        ap.target.date_filter.as_deref(),
        Some("24h"),
        "date_filter must remain \"24h\" when marker is present"
    );
}

#[test]
fn relax_legacy_filters_once_does_not_write_marker_when_persist_fails() {
    use tempfile::TempDir;

    // Force write_to_disk to fail: create a DIRECTORY at the data_file path
    // (autopilots.json). std::fs::write() to a path that is a directory fails
    // on every platform. The marker's parent dir remains writable, so the only
    // thing that can gate the marker write is whether write_to_disk returned Ok.
    let temp = TempDir::new().unwrap();
    let dir = temp.path().to_path_buf();

    // Create <dir>/autopilots.json as a directory, not a file.
    let data_file = dir.join("autopilots.json");
    std::fs::create_dir_all(&data_file).unwrap();

    // AutopilotStore::new expects the *parent* dir to exist, which it does (temp).
    // Passing `dir` means data_file = dir/autopilots.json — already a dir above.
    let store = AutopilotStore::new(&dir);

    // load() will return an empty map (can't read a dir as JSON), which is fine —
    // we just need write_to_disk to fail so the marker is NOT written.
    store.relax_legacy_filters_once();

    let marker = dir.join(RELAX_MARKER_FILE);
    assert!(
        !marker.exists(),
        "marker must NOT be written when write_to_disk fails (retry guarantee)"
    );
}
