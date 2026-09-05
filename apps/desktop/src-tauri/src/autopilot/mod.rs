use parking_lot::Mutex;
/// AutopilotStore — JSON-file-backed CRUD for Autopilot records.
///
/// Records are persisted to <dataDir>/autopilots.json as a flat JSON array.
/// All field names are serialised in camelCase to match the TypeScript schema
/// (`#[serde(rename_all = "camelCase")]`).
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::scraping::cluster::{
    assign_clusters, new_cluster_count, ClusterAssignment, ClusterInput, ClusterMemberRef,
};

// ── Back-compat deserializer: `board` (string) OR `boards` (array) → Vec<String> ──

/// Accept either a JSON string (`"board": "linkedin"`) or a JSON array
/// (`"boards": ["linkedin","remotive"]`) and normalise to `Vec<String>`.
///
/// Backward-compatibility deserializer: on-disk `autopilots.json` records written
/// before the multi-board change store a single string under the legacy `"board"`
/// key; new records store an array under `"boards"`. The
/// `#[serde(alias = "board")]` on the field lets the old key name be accepted by
/// serde before this function is called — no data migration or rewrite required.
fn string_or_vec<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct StringOrVec;

    impl<'de> Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string or a sequence of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_string()])
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(vec![v])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                out.push(s);
            }
            Ok(out)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            serde::Deserialize::deserialize(d).map(|v: serde_json::Value| match v {
                serde_json::Value::String(s) => vec![s],
                serde_json::Value::Array(arr) => arr
                    .into_iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect(),
                _ => Vec::new(),
            })
        }
    }

    de.deserialize_any(StringOrVec)
}

// ── Lenient deserializer: unrecognised workTypes entries are dropped ──────────

/// `Option<Vec<WorkType>>` that DROPS an unrecognised entry rather than
/// failing the whole field — mirrors [`string_or_vec`]'s back-compat posture,
/// one level down (an entry, not the field). A missing key deserializes as
/// `None` via `#[serde(default)]` on the field before this function is ever
/// called; an array left empty after dropping is `Some(vec![])`, which reads
/// as "no filter" the same as `None` does everywhere this is consumed.
fn parse_work_types_lenient<'de, D>(
    de: D,
) -> Result<Option<Vec<crate::scraping::types::WorkType>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<Vec<String>> = Option::deserialize(de)?;
    Ok(raw.map(|values| {
        let parsed: Vec<crate::scraping::types::WorkType> =
            values.iter().filter_map(|s| s.parse().ok()).collect();
        let dropped = values.len() - parsed.len();
        if dropped > 0 {
            // Count only — never the raw unrecognised string (see
            // `AutopilotStore::load`'s own `[autopilot]`-prefixed warn just
            // above for the sibling convention). This is the ONLY trace a
            // future vocabulary rename leaves before a persisted autopilot's
            // filter silently widens to "any" (an all-dropped array reads as
            // `Some(vec![])`, identical to no filter — see
            // `BoardSearchInput::work_type_spec`).
            log::warn!(
                "[autopilot] dropped {dropped} unrecognised workTypes entr{} while \
                 loading a persisted target",
                if dropped == 1 { "y" } else { "ies" }
            );
        }
        parsed
    }))
}

use crate::db::now_ms;
use crate::error::AppResult;
use crate::observability::sanitize_reason;

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutopilotTarget {
    /// The boards to scrape. Accepts either a `"boards": [...]` array (new
    /// format) or a `"board": "..."` string (legacy on-disk format). The alias
    /// + custom deserializer normalise both to `Vec<String>` transparently.
    #[serde(alias = "board", deserialize_with = "string_or_vec")]
    pub boards: Vec<String>,
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    /// Requested work arrangement(s) — empty/absent means "no filter" ("any").
    /// Unrecognised entries are DROPPED, not a hard parse failure (see
    /// [`parse_work_types_lenient`]). This matters on THREE paths, in
    /// increasing order of consequence:
    /// - `AutopilotStore::create`'s `unwrap_or_else` (a hard target parse
    ///   failure falls back to an empty target — only `work_types` would be
    ///   affected either way, since it is the one field that never hard-fails).
    /// - `AutopilotStore::update`'s patch merge silently ignores the WHOLE
    ///   target patch on a parse failure — every other edited field in the
    ///   same call is lost too, not just this one.
    /// - **The strongest reason, and the one that makes this load-bearing
    ///   rather than a nicety:** `AutopilotStore::load` deserializes each
    ///   persisted record individually and `filter_map`s out any that fail —
    ///   logged, never panicked — and the very next `save()` rewrites
    ///   `autopilots.json` WITHOUT that record. A single unrecognised
    ///   `workTypes` entry surviving to a hard parse failure would silently
    ///   delete an entire autopilot, including `found_jobs` and
    ///   `last_run_summaries`, on the next write. `#[serde(default)]` so a
    ///   pre-existing `autopilots.json` record with no `workTypes` key at
    ///   all — every record on disk today, since the only UI control that
    ///   could ever set this was removed in PR #614 before this field
    ///   existed — still deserializes.
    #[serde(
        default,
        deserialize_with = "parse_work_types_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub work_types: Option<Vec<crate::scraping::types::WorkType>>,
    pub pages: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_filter: Option<String>,
    /// How many top-scoring postings autopilot should apply to after scraping.
    /// Defaults to 3 when absent.
    #[serde(default = "default_top_n")]
    pub top_n: u32,
    /// Watched-companies-only mode (ADR-030 §e): when `Some(true)`, a run resolves
    /// the user's currently-starred discovered companies at run time and scrapes
    /// only those per-ATS company slugs (instead of the curated seed). Additive +
    /// optional so old records deserialize unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watched_companies_only: Option<bool>,
}

fn default_top_n() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutopilotFilter {
    pub min_match_score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_keywords: Option<Vec<String>>,
}

/// A job posting surfaced by an autopilot run. Lightweight summary persisted so
/// the user can review what each autopilot found.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundJob {
    pub title: String,
    pub company: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Board id the posting was scraped from (its JobPosting.source). Persisted so the
    /// apply flow records accurate per-job provenance for multi-board autopilots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    /// Full job description — used to pre-fill a tailored resume/cover letter generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Scraped salary range from the board (Adzuna only, today) — grounds the salary
    /// application answer before it falls back to a web lookup. `None` when the
    /// board doesn't expose salary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salary_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salary_max: Option<f64>,
    /// ISO-4217 currency for `salary_min`/`salary_max`. `None` when the board didn't
    /// report one (e.g. an Adzuna market not in the country→currency map).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salary_currency: Option<String>,
    /// Match score (0–100) when the posting passed ranking; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// The [`Self::score`] was computed over a TRUNCATED snippet (an aggregator/
    /// Adzuna description, which the board caps and the detail pane re-resolves
    /// to full text), so it may diverge from the detail pane's full-text
    /// re-score — the renderer marks such scores as provisional. `false` for
    /// full-text boards and for unscored jobs. Set at find-time in
    /// `commands::autopilot::build_found_job`. `#[serde(default)]` so a record
    /// written before this field existed loads as `false`.
    #[serde(default)]
    pub score_provisional: bool,
    /// WHICH kernel produced [`Self::score`] — the free keyword-coverage
    /// prefilter, or the combined semantic+ATS kernel the Jobs page uses
    /// (ADR-020 addendum). Set per job, so a posting whose embed failed and
    /// degraded back to keyword-only is labelled honestly even in a run where
    /// every other job re-ranked. Drives the renderer's metric label + band
    /// thresholds; `#[serde(default)]` makes every pre-existing record load as
    /// [`ScoreSource::Keyword`], which is exactly what it holds.
    #[serde(default)]
    pub score_source: ScoreSource,
    pub found_at: u64,
    /// The posting's publish-or-last-updated date (epoch ms), as reported by
    /// the board, copied from `JobPosting.posted_at` at find-time — distinct
    /// from [`Self::found_at`] (when WE scraped it). Most sources report a
    /// genuine creation/publish date (Adzuna, JSearch, the Apify LinkedIn
    /// actor, Arbeitnow, Ashby, Breezy, Jobicy, Lever, GermanTechJobs,
    /// BerlinStartupJobs, WeWorkRemotely, RemoteOK, Remotive, the HN "Who's
    /// Hiring" feed); a few (Jooble, Comeet, the Bundesagentur für Arbeit)
    /// only expose an "updated"/"current" timestamp upstream, so this can
    /// read as more recent than the posting's true original publish date for
    /// those. A board with no date field at all leaves it `None`.
    /// `#[serde(default)]` so a record written before this field existed
    /// loads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posted_at: Option<i64>,
    /// First surfaced in the most recent run (set by the dedup merge in
    /// [`AutopilotStore::record_run`]). Drives the "New" badge.
    #[serde(default)]
    pub is_new: bool,
    /// Whether the user has generated an application for this job. **Derived** at
    /// read time from a saved generation whose `job_url` matches `url` (see
    /// `commands::autopilot`), never hand-set, so it can't drift. Stored value is
    /// always `false`; the read path fills it in.
    #[serde(default)]
    pub applied: bool,
    /// Ghost-job trust signal, computed at find-time via
    /// [`crate::scraping::trust::assess_trust`]. `Option` (not a plain
    /// [`crate::scraping::trust::TrustAssessment`]) purely so a run recorded
    /// before this field existed still deserializes — `#[serde(default)]` gives
    /// `None` for a legacy record; every run recorded from here on always sets
    /// `Some(..)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<crate::scraping::trust::TrustAssessment>,
    /// Optional AI-reasoned note (2–4 sentences: why the job fits the résumé +
    /// one tailoring tip) generated for the top matches of a run when the
    /// autopilot has AI notes enabled (`assistant`). `None` for jobs not
    /// annotated (below the top-N ceiling, notes disabled, no provider, or the
    /// daily ceiling was hit mid-run). Read-only — never applied or submitted.
    /// `#[serde(default)]` so a job recorded before this field existed loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_notes: Option<String>,
    // ── Cross-board cluster annotations (ADR-029) ──────────────────────────────
    // Recomputed at every `record_run` (and on a `dedup_mark_not_duplicate`
    // split) by the pure `scraping::cluster` pass; never hand-set. All
    // serde-defaulted so a record written before clustering existed loads
    // unchanged (`cluster_id` None, `cluster_canonical` true = standalone,
    // no members, not an agency).
    /// The cluster this job belongs to (the canonical member's `merge_key`).
    /// `None` only on a legacy record not yet re-clustered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_id: Option<String>,
    /// Whether this job is its cluster's canonical (displayed) member. Defaults
    /// to `true` so a legacy/standalone job renders as its own canonical row.
    #[serde(default = "default_true")]
    pub cluster_canonical: bool,
    /// Every member of this job's cluster (`{key, board?, url}`), so the renderer
    /// can group + echo keys back to `dedup_mark_not_duplicate`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cluster_members: Vec<ClusterMemberRef>,
    /// Whether the posting's company is a recruiting/staffing agency (ADR-029 §i),
    /// computed at ingest from the built-in list + the user's extras.
    #[serde(default)]
    pub is_agency: bool,
}

/// Serde default for [`FoundJob::cluster_canonical`] — a job with no cluster
/// annotation reads as its own canonical row.
fn default_true() -> bool {
    true
}

/// Which scoring kernel produced a [`FoundJob::score`].
///
/// `Keyword` is the default in every sense: it is what the embedding-free
/// prefilter produces, what every record written before this enum existed
/// holds, and what a job degrades back to when its embed fails — so `Combined`
/// is only ever set by an explicit, successful semantic re-rank.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ScoreSource {
    /// Embedding-free keyword coverage (`documents::keywords::coverage_score`).
    #[default]
    Keyword,
    /// The Jobs-page combined semantic+ATS kernel
    /// (`commands::match_resume::score_one`).
    Combined,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AutopilotStatus {
    Active,
    Paused,
    Archived,
}

/// Outcome of the most recent run. Distinct from [`AutopilotStatus`] (the
/// agent's enabled/paused lifecycle): this tracks a single run so the UI can
/// show a live/failed/interrupted indicator.
///
/// `Interrupted` is not set by a run — it's reconciled at startup from a run
/// left `InProgress` when the app closed or crashed mid-run (see
/// [`AutopilotStore::mark_interrupted_runs`]).
///
/// `Completed`/`CompletedWithErrors`/`Failed` are derived from the run's
/// per-board summaries by [`derive_run_status`], so an all-boards-failed run no
/// longer masquerades as a clean `Completed, 0 found`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    InProgress,
    Completed,
    /// The run finished and at least one board returned results, but at least
    /// one other board errored or kept only a partial (`truncated`) harvest.
    CompletedWithErrors,
    Failed,
    Interrupted,
}

/// Derive the honest outcome of a run from its per-board summaries.
///
/// - `Failed` — **zero** boards succeeded (every board errored or was skipped).
///   The run couldn't actually do its job, so it must not read as a clean
///   completion.
/// - `CompletedWithErrors` — at least one board succeeded, but at least one
///   other board errored or kept only a partial (`truncated`) harvest. Results
///   are real but incomplete.
/// - `Completed` — at least one board succeeded and none errored or truncated.
///   A `skipped` board alone does not downgrade the status: a skip
///   (`needs-login`/`needs-company`/`needs-keys`) is an expected no-op, not a
///   failure of a board that ran.
///
/// A board "succeeded" when it neither errored nor was skipped; a `truncated`
/// board counts as a partial success (it did return rows). An empty slice is
/// treated as `Completed` — nothing reported a problem — preserving the
/// pre-summaries behavior for the degenerate no-boards case.
pub(crate) fn derive_run_status(summaries: &[crate::scraping::BoardScrapeSummary]) -> RunStatus {
    if summaries.is_empty() {
        return RunStatus::Completed;
    }
    let succeeded = summaries
        .iter()
        .filter(|s| s.error.is_none() && s.skipped.is_none())
        .count();
    if succeeded == 0 {
        return RunStatus::Failed;
    }
    let any_incomplete = summaries
        .iter()
        .any(|s| s.error.is_some() || s.truncated.is_some());
    if any_incomplete {
        RunStatus::CompletedWithErrors
    } else {
        RunStatus::Completed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Autopilot {
    #[serde(rename = "_id")]
    pub id: String,
    pub name: String,
    pub status: AutopilotStatus,
    pub target: AutopilotTarget,
    pub filter: AutopilotFilter,
    pub schedule: String,
    /// Local clock hour (0–23) a recurring schedule fires at. Used by
    /// daily/twice_daily; ignored by hourly. `None` falls back to 09:00 in the
    /// scheduler. Defaulted so older persisted records load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_hour: Option<u32>,
    /// Local clock minute (0–59) a recurring schedule fires at. Used by
    /// daily/twice_daily and as the "minute past the hour" for hourly. `None`
    /// falls back to minute 0 in the scheduler. Defaulted so older records load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_minute: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_text: Option<String>,
    /// Optional base cover letter reused as the starting point when tailoring a
    /// found job in the apply assistant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_letter: Option<String>,
    /// Opt-in (Phase 4): after the keyword rank, attach a short AI-reasoned note
    /// to the top matches of each run. Read-only enrichment — never applies or
    /// submits anything. `#[serde(default)]` so existing records load as `false`.
    #[serde(default)]
    pub assistant: bool,
    /// Provider/model/base-URL snapshot the headless AI-notes run resolves through
    /// the centralized [`crate::pipeline::Completer`] (the same layer `ai_generate`
    /// uses). The scheduler has no renderer to read the active provider from, so the
    /// one chosen at opt-in time is persisted here. `None`/empty → notes skip
    /// gracefully for that run. Defaulted so older records load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_base_url: Option<String>,
    pub total_found: u32,
    pub total_applied: u32,
    /// Jobs surfaced by the most recent run. Defaulted so older records load.
    #[serde(default)]
    pub found_jobs: Vec<FoundJob>,
    /// Outcome of the most recent run. `None` until the first run. Drives the
    /// live/failed/interrupted badge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_status: Option<RunStatus>,
    /// Per-board outcome of the most recent run (board, count, error, skipped,
    /// truncated). Persisted so the UI can explain a zero/partial result *after*
    /// the run — until now these summaries were computed at the record site and
    /// discarded. `#[serde(default)]` so records written before this field
    /// existed load as an empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_run_summaries: Vec<crate::scraping::BoardScrapeSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

// ── Store ─────────────────────────────────────────────────────────────────────

pub struct AutopilotStore {
    data_file: PathBuf,
    cache: Mutex<Option<HashMap<String, Autopilot>>>,
}

impl AutopilotStore {
    pub fn new(data_dir: &PathBuf) -> Self {
        std::fs::create_dir_all(data_dir).ok();
        Self {
            data_file: data_dir.join("autopilots.json"),
            cache: Mutex::new(None),
        }
    }

    // ── CRUD ──────────────────────────────────────────────────────────────────

    pub fn list(&self) -> Vec<Autopilot> {
        let map = self.load();
        let mut items: Vec<Autopilot> = map.into_values().collect();
        items.sort_by(cmp_autopilot_newest_first);
        items
    }

    pub fn get(&self, id: &str) -> Option<Autopilot> {
        self.load().remove(id)
    }

    pub fn create(&self, input: serde_json::Value) -> Autopilot {
        let now = now_ms();
        let ap = Autopilot {
            id: Uuid::new_v4().to_string(),
            name: str_field(&input, "name"),
            status: AutopilotStatus::Active,
            target: serde_json::from_value(input["target"].clone()).unwrap_or_else(|_| {
                AutopilotTarget {
                    boards: Vec::new(),
                    query: String::new(),
                    location: None,
                    country_code: None,
                    work_types: None,
                    pages: 1,
                    date_filter: None,
                    top_n: default_top_n(),
                    watched_companies_only: None,
                }
            }),
            filter: serde_json::from_value(input["filter"].clone()).unwrap_or({
                // Creation default when no explicit filter is supplied: keep
                // everything (0.0). A non-zero default silently dropped jobs a
                // manual search would have returned — the autopilot zero-jobs bug.
                AutopilotFilter {
                    min_match_score: 0.0,
                    keywords: None,
                    exclude_keywords: None,
                }
            }),
            schedule: str_field(&input, "schedule"),
            schedule_hour: u32_field_in_range(&input, "scheduleHour", 23),
            schedule_minute: u32_field_in_range(&input, "scheduleMinute", 59),
            resume_text: input["resumeText"].as_str().map(String::from),
            cover_letter: input["coverLetter"].as_str().map(String::from),
            assistant: input["assistant"].as_bool().unwrap_or(false),
            assistant_provider: input["assistantProvider"].as_str().map(String::from),
            assistant_model: input["assistantModel"].as_str().map(String::from),
            assistant_base_url: input["assistantBaseUrl"].as_str().map(String::from),
            total_found: 0,
            total_applied: 0,
            found_jobs: Vec::new(),
            run_status: None,
            last_run_summaries: Vec::new(),
            last_run_at: None,
            created_at: now,
            updated_at: now,
        };
        let mut map = self.load();
        map.insert(ap.id.clone(), ap.clone());
        self.save(map);
        ap
    }

    pub fn update(&self, id: &str, patch: serde_json::Value) -> Option<Autopilot> {
        let mut map = self.load();
        let ap = map.get_mut(id)?;
        ap.updated_at = now_ms();
        if let Some(v) = patch.get("name").and_then(|v| v.as_str()) {
            ap.name = v.to_string();
        }
        if let Some(v) = patch.get("status").and_then(|v| v.as_str()) {
            ap.status = match v {
                "paused" => AutopilotStatus::Paused,
                "archived" => AutopilotStatus::Archived,
                _ => AutopilotStatus::Active,
            };
        }
        if let Ok(t) = serde_json::from_value::<AutopilotTarget>(patch["target"].clone()) {
            ap.target = t;
        }
        if let Ok(f) = serde_json::from_value::<AutopilotFilter>(patch["filter"].clone()) {
            ap.filter = f;
        }
        if let Some(v) = patch.get("schedule").and_then(|v| v.as_str()) {
            ap.schedule = v.to_string();
        }
        if patch.get("scheduleHour").is_some() {
            ap.schedule_hour = u32_field_in_range(&patch, "scheduleHour", 23);
        }
        if patch.get("scheduleMinute").is_some() {
            ap.schedule_minute = u32_field_in_range(&patch, "scheduleMinute", 59);
        }
        if let Some(v) = patch.get("resumeText").and_then(|v| v.as_str()) {
            ap.resume_text = Some(v.to_string());
        }
        if let Some(v) = patch.get("coverLetter").and_then(|v| v.as_str()) {
            ap.cover_letter = Some(v.to_string());
        }
        if let Some(v) = patch.get("assistant").and_then(|v| v.as_bool()) {
            ap.assistant = v;
            if !v {
                // Toggling AI notes off: clear the stale provider/model/base-url
                // snapshot too. The renderer omits `assistantProvider`/`Model`/
                // `BaseUrl` from the patch when disabling, so without this the old
                // snapshot would linger invisibly and could be reused verbatim if
                // AI notes are re-enabled later without a fresh provider pick.
                ap.assistant_provider = None;
                ap.assistant_model = None;
                ap.assistant_base_url = None;
            }
        }
        // The provider snapshot travels together with the toggle: the renderer
        // writes all three when the user enables AI notes (from the active
        // provider), so a re-selected provider re-snapshots on the next update.
        if let Some(v) = patch.get("assistantProvider").and_then(|v| v.as_str()) {
            ap.assistant_provider = Some(v.to_string());
        }
        if let Some(v) = patch.get("assistantModel").and_then(|v| v.as_str()) {
            ap.assistant_model = Some(v.to_string());
        }
        if let Some(v) = patch.get("assistantBaseUrl").and_then(|v| v.as_str()) {
            ap.assistant_base_url = Some(v.to_string());
        }
        let result = ap.clone();
        self.save(map);
        Some(result)
    }

    pub fn remove(&self, id: &str) {
        let mut map = self.load();
        map.remove(id);
        self.save(map);
    }

    /// Remove every autopilot and its found-jobs history (factory reset).
    pub fn clear_all(&self) {
        self.save(HashMap::new());
    }

    pub fn set_status(&self, id: &str, status: AutopilotStatus) {
        let mut map = self.load();
        if let Some(ap) = map.get_mut(id) {
            ap.status = status;
            ap.updated_at = now_ms();
        }
        self.save(map);
    }

    /// Set the most-recent-run outcome. `InProgress` is set at run start;
    /// `record_run` derives `Completed`/`CompletedWithErrors`/`Failed` from the
    /// per-board summaries for a run that reaches the record site.
    pub fn set_run_status(&self, id: &str, status: RunStatus) {
        let mut map = self.load();
        if let Some(ap) = map.get_mut(id) {
            ap.run_status = Some(status);
            ap.updated_at = now_ms();
        }
        self.save(map);
    }

    /// Set the run outcome for a run that has NO fresh summaries to report
    /// (never reached `record_run`) — ALSO clears `last_run_summaries` so a
    /// stale chip strip from the PRIOR run doesn't render as if it belonged to
    /// this one. Two callers today: an outright scrape error (`Failed`, no
    /// summaries were ever collected) and a user-cancelled run (`Completed`,
    /// the in-flight summaries were never finalized/recorded either).
    pub fn set_run_status_clearing_summaries(&self, id: &str, status: RunStatus) {
        let mut map = self.load();
        if let Some(ap) = map.get_mut(id) {
            ap.run_status = Some(status);
            ap.last_run_summaries = Vec::new();
            ap.updated_at = now_ms();
        }
        self.save(map);
    }

    /// Mark a run `Failed` on an outright scrape error. Thin wrapper over
    /// [`Self::set_run_status_clearing_summaries`] kept as a named entry point
    /// for the one fixed outcome that path always sets.
    pub fn fail_run_without_summaries(&self, id: &str) {
        self.set_run_status_clearing_summaries(id, RunStatus::Failed);
    }

    /// Reconcile runs left mid-flight: any autopilot still marked `InProgress`
    /// when the app starts was interrupted by a crash or close, so flip it to
    /// `Interrupted` for an honest badge instead of a stuck "running" state.
    /// Returns the ids reconciled, so the scheduler can schedule a single
    /// bounded recovery retry for the ones whose scheduled occurrence hasn't
    /// since rolled (see `autopilot_scheduler`). Called once at startup.
    pub fn mark_interrupted_runs(&self) -> Vec<String> {
        let mut map = self.load();
        let mut reconciled = Vec::new();
        for ap in map.values_mut() {
            if ap.run_status == Some(RunStatus::InProgress) {
                ap.run_status = Some(RunStatus::Interrupted);
                ap.updated_at = now_ms();
                reconciled.push(ap.id.clone());
            }
        }
        if !reconciled.is_empty() {
            self.save(map);
        }
        reconciled
    }

    /// Persist the outcome of a run: counts, last-run time, and the found-jobs
    /// list **merged** with prior runs by URL — so re-running keeps history
    /// (first-seen + any state) instead of replacing it, and genuinely new
    /// postings are flagged `is_new`.
    /// Returns the number of **newly surfaced** jobs in this run (postings whose
    /// URL was never seen before) — drives the "N new jobs" notification + tray.
    ///
    /// `summaries` are the per-board outcomes of the run: they are persisted on
    /// the record (so the UI can explain a zero/partial result after the run)
    /// and drive the derived [`RunStatus`] via [`derive_run_status`] — an
    /// all-boards-failed run now records `Failed`, a mixed run
    /// `CompletedWithErrors`, instead of a blanket `Completed`.
    pub fn record_run(
        &self,
        id: &str,
        total_found: u32,
        total_applied: u32,
        found_jobs: Vec<FoundJob>,
        summaries: Vec<crate::scraping::BoardScrapeSummary>,
        tombstones: &HashSet<(String, String)>,
        extra_agency: &[String],
    ) -> u32 {
        let mut map = self.load();
        let mut new_count = 0u32;
        if let Some(ap) = map.get_mut(id) {
            let now = now_ms();
            ap.total_found = total_found;
            ap.total_applied = total_applied;
            ap.found_jobs = merge_found_jobs(&ap.found_jobs, found_jobs);
            // Cross-board cluster the FULL merged list, write cluster annotations
            // onto each row, and count clusters whose members are ALL first-seen
            // this run (ADR-029 §f) — a known job resurfacing on another board no
            // longer notifies. `merge_found_jobs` set `is_new` per canonical key;
            // that set drives which clusters count as new.
            let new_keys: HashSet<String> = ap
                .found_jobs
                .iter()
                .filter(|j| j.is_new)
                .map(merge_key)
                .collect();
            let assignments = cluster_found_jobs(&mut ap.found_jobs, tombstones, extra_agency);
            new_count = new_cluster_count(&assignments, &new_keys);
            ap.run_status = Some(derive_run_status(&summaries));
            // Strip the Track B1 `health` before persisting. It is a DISPLAY-TIME
            // derivation of the live `board_health` store, not state belonging to
            // this run: freezing it here would (a) show a verdict that stopped
            // being true the moment the next run landed, and (b) leak it into the
            // backup bundle — `AutopilotStore::export` writes `lastRunSummaries`
            // verbatim, so importing on another machine would replay THIS
            // machine's failure streaks, timestamps, last error and run id as if
            // that machine had lived them. The store itself is deliberately not a
            // `DataStore` for exactly that reason; this closes the side door.
            ap.last_run_summaries = summaries
                .into_iter()
                .map(|mut s| {
                    s.health = None;
                    s
                })
                .collect();
            ap.last_run_at = Some(now);
            ap.updated_at = now;
        }
        self.save(map);
        new_count
    }

    /// Recompute + persist cluster annotations for ONE record's found-jobs after
    /// a tombstone change (`dedup_mark_not_duplicate`), leaving counts/run status
    /// untouched. No-op for an unknown id. The split takes effect immediately and
    /// — because clustering is recomputed every run — survives future re-scrapes.
    pub fn recompute_record_clusters(
        &self,
        id: &str,
        tombstones: &HashSet<(String, String)>,
        extra_agency: &[String],
    ) {
        let mut map = self.load();
        let mut changed = false;
        if let Some(ap) = map.get_mut(id) {
            cluster_found_jobs(&mut ap.found_jobs, tombstones, extra_agency);
            ap.updated_at = now_ms();
            changed = true;
        }
        if changed {
            self.save(map);
        }
    }

    pub fn stamp_last_run(&self, id: &str) {
        let mut map = self.load();
        if let Some(ap) = map.get_mut(id) {
            ap.last_run_at = Some(now_ms());
            ap.updated_at = now_ms();
        }
        self.save(map);
    }

    /// Patch `description` on every [`FoundJob`] — across EVERY autopilot
    /// record, not just one — whose own `url` normalizes to `normalized_url`
    /// (issue #1106 part b). `PostingsCache` (`postings::mod`) is a
    /// completely disjoint, session-lifetime store that a posting surfaced
    /// via `job`/`best-matches`/`autopilot_best_matches` never touches (those
    /// resources read `found_jobs` directly — see `agent_read`'s module
    /// doc); a correction from `commands::scrape::scrape_update_description`
    /// must reach this store too or the exact case issue #1106 reports (a
    /// correction that silently doesn't stick) stays unfixed. Same
    /// load-mutate-save shape as [`Self::recompute_record_clusters`] just
    /// above, but over every record and every matching row within it — the
    /// same posting can legitimately appear more than once across different
    /// autopilots. Returns the number of rows updated (`0` when nothing
    /// matched, so the caller can tell it found nothing to correct).
    ///
    /// Also recomputes `trust` (via [`crate::scraping::trust::assess_trust`],
    /// the same three-arg call `commands::autopilot::build_found_job` makes)
    /// — `trust` is genuinely description-scoped, so a correction from an
    /// empty/title-only description to a real one clears the stale
    /// `DescriptionUnavailable` flag immediately instead of leaving it
    /// contradicting the now-visible full text until the next scrape
    /// re-derives it (issue #1106).
    ///
    /// `score_provisional` is deliberately left UNTOUCHED here. It describes
    /// the `score` field (see `packages/shared/src/types/index.ts`'s doc
    /// comment on `MatchScoreSummary.provisional`), and — per the doc comment
    /// above — the numeric `score` itself is NOT recomputed by a manual text
    /// correction, so nothing about the flag's meaning has changed either. A
    /// prior version of this function derived `score_provisional` from the
    /// NEW description's blank-ness, which asserted freshness of a number
    /// that never moved (the exact dishonesty issue #1105 exists to close).
    /// It only goes back to `false` once an actual autopilot run re-scores
    /// the corrected content.
    pub fn update_found_job_descriptions(&self, normalized_url: &str, description: &str) -> u32 {
        let mut map = self.load();
        let mut updated = 0u32;
        for ap in map.values_mut() {
            let mut changed = false;
            for job in &mut ap.found_jobs {
                if crate::applications::normalize_job_url(&job.url) == normalized_url {
                    job.description = Some(description.to_string());
                    job.trust = Some(crate::scraping::trust::assess_trust(
                        &job.url,
                        &job.company,
                        description,
                    ));
                    changed = true;
                    updated += 1;
                }
            }
            if changed {
                ap.updated_at = now_ms();
            }
        }
        if updated > 0 {
            self.save(map);
        }
        updated
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    fn load(&self) -> HashMap<String, Autopilot> {
        // Silent migration off auto-apply: records written before the apply engine
        // was removed carry `action` (save/review/auto_apply) and `autoSubmit`
        // fields. Serde ignores unknown fields on deserialize, so every saved
        // autopilot loads cleanly as a find-&-save agent and the dead keys are
        // dropped on the next save — no explicit rewrite needed.
        let mut guard = self.cache.lock();
        if let Some(ref c) = *guard {
            return c.clone();
        }
        // Per-record tolerant parse: deserialize the file as a `Vec<Value>` first,
        // then each record individually, so ONE record carrying an unknown/future
        // field value (e.g. a `runStatus` variant a downgraded build doesn't know,
        // like `completedWithErrors`) drops only that record instead of failing
        // the whole-`Vec<Autopilot>` parse — which previously produced an empty
        // map, and a later `save()` would silently overwrite the file, losing
        // every OTHER record too. Errors are counted and logged, never panicked.
        let raw: Vec<serde_json::Value> = std::fs::read_to_string(&self.data_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut dropped = 0usize;
        let mut map: HashMap<String, Autopilot> = raw
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<Autopilot>(v) {
                Ok(ap) => Some((ap.id.clone(), ap)),
                Err(e) => {
                    dropped += 1;
                    log::warn!("[autopilot] dropping unparseable record: {e}");
                    None
                }
            })
            .collect();
        if dropped > 0 {
            log::warn!("[autopilot] load: dropped {dropped} unparseable record(s)");
        }
        // Scrub a THIRD sink for the Track B1 board-health verdict (see
        // `strip_board_health`'s doc): an on-disk file from an intermediate
        // build predating the `record_run`/`export`/`import` strips, or a
        // hand-edited one. This cache is a `save()`'s worth of one round trip
        // away from `record_run`'s own strip too — any OTHER mutation
        // (`set_run_status`, `stamp_last_run`, …) re-serializes this map
        // untouched, so a record that entered here with stale health would
        // otherwise keep re-persisting it forever instead of aging out.
        strip_board_health(map.values_mut());
        *guard = Some(map.clone());
        map
    }

    fn save(&self, map: HashMap<String, Autopilot>) {
        // A persistent-write failure must be LOUD, not swallowed: the in-memory
        // cache below would otherwise diverge from disk silently, and the next
        // reader (or a restart) would lose this state with no signal at all
        // (quick win 9). Smallest honest surface — a `log::error` with context;
        // deliberately NOT a retry queue. Migrations that need to *observe* a
        // successful persist still call `write_to_disk` directly.
        if let Err(e) = self.write_to_disk(&map) {
            log::error!(
                "[autopilot] failed to persist autopilots.json: {}",
                sanitize_reason(&e.to_string())
            );
        }
        *self.cache.lock() = Some(map);
    }

    /// Serialize + flush the map to `autopilots.json`, returning the IO outcome so
    /// a caller can gate on a successful persist (e.g. the one-shot migration's
    /// done-marker). Does NOT update the in-memory cache — that's `save`'s job.
    /// `Ok(())` is also returned on the no-op-write path (state already on disk).
    fn write_to_disk(&self, map: &HashMap<String, Autopilot>) -> std::io::Result<()> {
        let list: Vec<&Autopilot> = {
            let mut v: Vec<&Autopilot> = map.values().collect();
            v.sort_by(|a, b| cmp_autopilot_newest_first(a, b));
            v
        };
        let Ok(json) = serde_json::to_string_pretty(&list) else {
            // Serialization can't fail for this shape, but if it ever did there's
            // nothing on disk to trust — surface it as an IO-style error so the
            // migration won't mark itself done on un-persisted data.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "failed to serialize autopilots",
            ));
        };
        // No-op-write skip: many mutations (set_run_status, stamp_last_run, …)
        // re-serialize identical state. Skip the disk write when the bytes
        // match what's already persisted — a pure dirty check, NOT debouncing,
        // so state is still flushed synchronously the instant it changes (no
        // crash-loss window). A missing/unreadable file never matches → write.
        let unchanged = std::fs::read_to_string(&self.data_file)
            .map(|existing| existing == json)
            .unwrap_or(false);
        if unchanged {
            return Ok(()); // desired state already persisted
        }
        std::fs::write(&self.data_file, json)
    }

    /// Replace all autopilots with the given set (preserving their ids). Used by
    /// backup restore.
    pub fn replace_all(&self, items: Vec<Autopilot>) {
        let map: HashMap<String, Autopilot> =
            items.into_iter().map(|ap| (ap.id.clone(), ap)).collect();
        self.save(map);
    }

    /// One-shot, idempotent migration that loosens autopilots saved with the old
    /// auto-prefilled restrictive filters (the cause of "autopilot returns ZERO
    /// jobs while manual search returns jobs for the same query"). Runs once per
    /// install, gated by a sidecar marker file beside `autopilots.json` — NOT a
    /// schema_version field, so the persisted/IPC shape is unchanged and gen:ipc
    /// can't drift.
    ///
    /// If the marker already exists this returns immediately. Otherwise it loads
    /// every autopilot, applies [`relax_legacy_filters`] to each, saves once, and
    /// writes the marker. Synchronous file IO — call it from the setup path, never
    /// from an async worker.
    ///
    /// Lock safety: [`Self::load`] takes `self.cache.lock()` but drops the guard
    /// before returning a cloned map, and [`Self::save`] re-takes the lock. This
    /// method only ever holds the owned clone from `load()` across the `save()`
    /// call — no `load()` guard is alive when `save()` re-locks, so there is no
    /// re-entrant lock / deadlock.
    pub fn relax_legacy_filters_once(&self) {
        let marker = self
            .data_file
            .parent()
            .map(|p| p.join(RELAX_MARKER_FILE))
            .unwrap_or_else(|| PathBuf::from(RELAX_MARKER_FILE));
        if marker.exists() {
            return;
        }

        let mut map = self.load(); // owned clone; the cache guard is already dropped
        for ap in map.values_mut() {
            relax_legacy_filters(ap);
        }

        // Persist FIRST and observe the result. Only write the done-marker once the
        // relaxed data is known to have hit disk — otherwise a save failure plus a
        // successful marker write would leave autopilots restrictive forever
        // ("done" but never relaxed). On a save error we skip the marker, the cache
        // stays as-loaded, and the next launch retries (harmless — the pass is
        // idempotent). `write_to_disk` returns Ok on the no-op path too (state
        // already persisted), which is also a valid "done" condition.
        let persisted = self.write_to_disk(&map);
        // Keep the in-memory cache consistent with whatever we just (attempted to)
        // persist; on success this is the relaxed map, mirroring `save`.
        *self.cache.lock() = Some(map);

        if persisted.is_ok() {
            // Mark done even if some autopilots were already loose: the goal is to
            // run the loosen pass exactly once, not to gate on whether it changed
            // anything.
            if let Some(parent) = self.data_file.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&marker, b"1").ok();
        }
    }
}

/// Sidecar marker that records the legacy-filter loosen migration ran. Lives
/// beside `autopilots.json` in the data dir; its presence makes
/// [`AutopilotStore::relax_legacy_filters_once`] a no-op on subsequent launches.
const RELAX_MARKER_FILE: &str = "autopilot_relax_v1.done";

/// Surgically loosen one autopilot's auto-prefilled restrictive filters in place.
/// Kills the zero-jobs bug while preserving deliberate user customizations:
///
/// - `filter.keywords` → `None` **only on a legacy record** (see below),
/// - `filter.min_match_score` → `0.0` **only if** it is still the old auto
///   default `50.0`,
/// - `target.date_filter` → `None` **only if** it is still the old auto default
///   `Some("24h")`.
///
/// Everything else (`exclude_keywords`, `query`, `location`, `country_code`,
/// `boards`, `pages`, `work_types`, `top_n`) is left untouched. Pure + filesystem-
/// free so it is unit-testable on a bare `&mut Autopilot`.
///
/// **Idempotency guarantee.** "Legacy" is decided up front from the two *sentinel*
/// fields (`min_match_score == 50.0` OR `date_filter == Some("24h")`) — the
/// prefilled `keywords` clear is gated on that flag, NOT applied unconditionally.
/// So a record that's already been relaxed (score `0.0`, date `None`) is NOT
/// legacy → the whole function is a no-op → re-running can never erase keywords
/// the user added after the first relaxation. This matters because the done-marker
/// write in [`AutopilotStore::relax_legacy_filters_once`] is best-effort
/// (`.ok()`-swallowed): if it fails, the migration re-runs on next launch, and
/// this no-op-on-relaxed property is what makes that rerun safe. The marker is now
/// purely an optimization, not a correctness gate.
///
/// Narrow accepted gap: a record with prefilled keywords where the user ALSO
/// changed *both* the score (≠50) *and* the date (≠"24h") reads as non-legacy, so
/// its keywords are kept. That's rare and diagnosable, and erring toward keeping
/// user data is the safe direction.
pub(crate) fn relax_legacy_filters(ap: &mut Autopilot) {
    // Decide legacy-ness from the sentinels BEFORE mutating them, so the decision
    // can't be invalidated by our own resets below.
    let was_legacy =
        ap.filter.min_match_score == 50.0 || ap.target.date_filter.as_deref() == Some("24h");

    if was_legacy {
        // Only legacy records carry the auto-prefilled keyword list manual search
        // never applies; clearing it on an already-relaxed record would erase
        // user-added keywords on a migration rerun.
        ap.filter.keywords = None;
    }

    if ap.filter.min_match_score == 50.0 {
        ap.filter.min_match_score = 0.0;
    }

    // `"24h"` is the ONLY restrictive legacy auto-default: the pre-#483 wizard's
    // `buildDefaults` set `dateFilter: '24h'`, while "any time" persisted as `None`
    // (`wizardStateToPayload` maps `'' → undefined`). A user-picked `'week'`/
    // `'month'` is therefore deliberate and left untouched.
    if ap.target.date_filter.as_deref() == Some("24h") {
        ap.target.date_filter = None;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Sort autopilots newest-first by `created_at`, breaking ties by `id` so the
/// order is stable across runs despite the unordered map. Single source of truth
/// for both the read order (`list`) and the on-disk order (`save`).
fn cmp_autopilot_newest_first(a: &Autopilot, b: &Autopilot) -> Ordering {
    b.created_at
        .cmp(&a.created_at)
        .then_with(|| a.id.cmp(&b.id))
}

fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Read an optional non-negative integer field as `u32`, accepting it only when
/// it is in `0..=max`. Absent, non-numeric, or out-of-range → `None`, so a
/// missing or poisoned schedule time falls back to the scheduler defaults
/// rather than persisting a value that makes the occurrence permanently `None`
/// (silently dead autopilot). Guards the storage boundary against a client that
/// bypassed the Zod range check (e.g. `scheduleHour: 25`).
fn u32_field_in_range(v: &serde_json::Value, key: &str, max: u32) -> Option<u32> {
    v.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .filter(|&n| n <= max)
}

/// Canonical dedup key for a [`FoundJob`] — a thin `FoundJob` adapter over the
/// app-wide [`canonical_job_key`](crate::scraping::boards::common::canonical_job_key),
/// the single source of truth for "is this the same job?" shared with the scrape
/// engine's cross-source pass and (mirrored in TS) the renderer's `mergePostings`.
/// Forwarding the posting's `url`/`title`/`company` keeps autopilot's merge keyed
/// byte-for-byte identically to those, so a job surfaced by two sources collapses to
/// one row and fires one notification — and persisted found-jobs keyed under the old
/// inlined copy recompute to the same key (the algorithm is unchanged, only DRYed).
/// The aggregator-redirect-vs-direct-URL limitation is documented on that fn.
fn merge_key(j: &FoundJob) -> String {
    crate::scraping::boards::common::canonical_job_key(&j.url, &j.title, &j.company)
}

/// Byte length of a job's description (0 when absent) — used to keep the richer of
/// two same-key postings when collapsing a within-batch duplicate.
fn description_len(j: &FoundJob) -> usize {
    j.description.as_deref().map(str::len).unwrap_or(0)
}

/// Merge a fresh run's postings into the cumulative found-jobs list, idempotently:
///
/// - the incoming batch is first collapsed on [`merge_key`], so the SAME job
///   surfaced twice in one batch, or via two tracking-param/hash URL variants,
///   becomes ONE row — and counts once in the "N new jobs" notification (this
///   does NOT collapse an aggregator's redirect URL against a board's direct
///   URL for the same job; see [`merge_key`]),
/// - existing rows are kept (preserving `found_at`/first-seen) and have `is_new`
///   cleared; if the run re-surfaced them (matched by `merge_key`), volatile
///   fields (title/company/description/score/location/salary/board/trust) refresh,
/// - postings whose key was never seen are placed first (on top) and flagged
///   `is_new`, in the incoming (already score-sorted) order.
///
/// Re-running with the same postings yields the same set — only `is_new` moves.
/// `applied` is derived on read, so it is left at its default here.
fn merge_found_jobs(existing: &[FoundJob], incoming: Vec<FoundJob>) -> Vec<FoundJob> {
    use std::collections::{HashMap, HashSet};

    // 1) Collapse duplicates WITHIN the incoming batch by canonical key. First
    //    occurrence keeps its position; a later duplicate carrying a longer
    //    description upgrades that one field (richer text for the tailor flow).
    //    (The engine's `dedup_cross_source` runs upstream over the FULL
    //    multi-board batch, keyed on the exact same `canonical_job_key` that
    //    `build_found_job` copies verbatim into `merge_key` (url/title/company
    //    are never mutated in between) — so via the one production caller
    //    (`commands::autopilot::autopilot_run`) this `Some(kept)` branch is
    //    never actually hit: every duplicate, same-source or cross-source, was
    //    already collapsed — including its `posted_at`, via the same
    //    fill-without-clobbering pattern this file uses — before `incoming` was
    //    built. This pass is `merge_found_jobs`'s own defensive contract for a
    //    caller that hands it pre-existing duplicates directly, e.g. a test.)
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, FoundJob> = HashMap::new();
    for job in incoming {
        let key = merge_key(&job);
        match by_key.get_mut(&key) {
            Some(kept) => {
                if description_len(&job) > description_len(kept) {
                    kept.description = job.description;
                }
            }
            None => {
                order.push(key.clone());
                by_key.insert(key, job);
            }
        }
    }
    let incoming: Vec<FoundJob> = order
        .into_iter()
        .map(|k| by_key.remove(&k).expect("key inserted above"))
        .collect();

    // 2) Merge the de-duplicated batch against existing rows, keyed the same way so
    //    a re-surfaced posting matches regardless of which URL variant/source
    //    persisted it.
    let incoming_by_key: HashMap<String, &FoundJob> =
        incoming.iter().map(|j| (merge_key(j), j)).collect();

    let refreshed_existing: Vec<FoundJob> = existing
        .iter()
        .map(|e| {
            let mut row = e.clone();
            row.is_new = false;
            if let Some(inc) = incoming_by_key.get(&merge_key(e)) {
                row.title = inc.title.clone();
                row.company = inc.company.clone();
                if inc.location.is_some() {
                    row.location = inc.location.clone();
                }
                // Carry the board over: existing rows persisted before `board` existed
                // (`None`) pick it up when the same job re-surfaces; the append branch
                // (`..inc`) preserves it for never-seen jobs.
                if inc.board.is_some() {
                    row.board = inc.board.clone();
                }
                // NOT `inc.description.is_some()` like the other fields above: a
                // LinkedIn search result always carries `Some("")` (never `None`)
                // for its description — that is its "unknown" sentinel, not
                // `None`. An `is_some()` guard would let that blank resurface
                // every run and clobber a real description the enrichment pass
                // (`autopilot_helpers::linkedin_enrich`) had already fetched and
                // written back — see
                // `merge_preserves_a_real_description_over_a_blank_resurface`.
                if !crate::documents::keywords::description_is_blank(inc.description.as_deref()) {
                    row.description = inc.description.clone();
                }
                // Same fill-without-clobbering pattern as `board`/`description`: an
                // existing row persisted before `posted_at` existed (`None`), or
                // whose board didn't expose a publish date on an earlier run, picks
                // it up when the same job re-surfaces with one known.
                if inc.posted_at.is_some() {
                    row.posted_at = inc.posted_at;
                }
                if inc.score.is_some() {
                    // Paired fields — `score_provisional` and `score_source`
                    // both describe WHICH score is on the row, so they must move
                    // with `score`, never be left stale from a prior source
                    // (e.g. a full-text board's authoritative score resurfacing
                    // over an old aggregator snippet score, or vice versa — a
                    // snippet score must never display as authoritative; and a
                    // run where the user turned semantic scoring OFF must not
                    // leave last run's "combined" label on a keyword number).
                    row.score = inc.score;
                    row.score_provisional = inc.score_provisional;
                    row.score_source = inc.score_source;
                }
                // Same fill-without-clobbering pattern as `board`/`description`: a
                // re-scrape that newly learns the salary updates the row, but never
                // overwrites an already-known value with an unknown one.
                if inc.salary_min.is_some() {
                    row.salary_min = inc.salary_min;
                }
                if inc.salary_max.is_some() {
                    row.salary_max = inc.salary_max;
                }
                if inc.salary_currency.is_some() {
                    row.salary_currency = inc.salary_currency.clone();
                }
                // Same legacy-migration case as `board` above: an existing row
                // persisted before `trust` existed (`None`) picks it up when the
                // same job re-surfaces on a later run.
                if inc.trust.is_some() {
                    row.trust = inc.trust.clone();
                }
            }
            row
        })
        .collect();

    let existing_keys: HashSet<String> = existing.iter().map(merge_key).collect();
    // New jobs go on top: the batch is already score-sorted desc upstream
    // (commands/autopilot.rs), so preserving incoming order here keeps that.
    let mut merged: Vec<FoundJob> = incoming
        .into_iter()
        .filter(|inc| !existing_keys.contains(&merge_key(inc)))
        .map(|inc| FoundJob {
            is_new: true,
            applied: false,
            ..inc
        })
        .collect();
    merged.extend(refreshed_existing);

    merged
}

// ── Cross-board clustering (ADR-029) ────────────────────────────────────────

/// Build [`ClusterInput`]s for a found-jobs list. `key` is the canonical
/// [`merge_key`] (the identity tombstones + the renderer share). Vectors are
/// always `None` on this path — a `FoundJob` has no posting id, so there is no
/// cached embedding to look up (the string path is structurally primary here,
/// ADR-029 §c). `pub(crate)` so the L3 retain pass reuses the exact projection.
///
/// Generic over anything that yields `&FoundJob` (a slice reference, `.iter()`,
/// or any other borrowing iterator) rather than `&[FoundJob]` specifically —
/// `commands::autopilot::best_matches::compute_best_matches` builds its union
/// as `Vec<&FoundJob>` (borrowing straight from the already-owned autopilot
/// records) so this projection never forces a second full-struct clone of
/// every found job just to feed it.
pub(crate) fn found_job_cluster_inputs<'a>(
    jobs: impl IntoIterator<Item = &'a FoundJob>,
) -> Vec<ClusterInput> {
    jobs.into_iter()
        .map(|j| ClusterInput {
            key: merge_key(j),
            title: j.title.clone(),
            company: j.company.clone(),
            url: j.url.clone(),
            source: j.board.clone(),
            has_description: j
                .description
                .as_deref()
                .is_some_and(|d| !d.trim().is_empty()),
            seen_at: j.found_at,
            vector: None,
            space: None,
        })
        .collect()
}

/// Cluster a found-jobs list IN PLACE: run [`assign_clusters`] and write each
/// verdict (`cluster_id`, `cluster_canonical`, `cluster_members`, `is_agency`)
/// onto the matching row by index. Returns the assignments so the caller can
/// derive the new-cluster count.
fn cluster_found_jobs(
    jobs: &mut [FoundJob],
    tombstones: &HashSet<(String, String)>,
    extra_agency: &[String],
) -> Vec<ClusterAssignment> {
    let inputs = found_job_cluster_inputs(jobs.iter());
    let assignments = assign_clusters(inputs, tombstones, extra_agency);
    for (job, assignment) in jobs.iter_mut().zip(assignments.iter()) {
        job.cluster_id = Some(assignment.cluster_id.clone());
        job.cluster_canonical = assignment.canonical;
        job.cluster_members = assignment.members.clone();
        job.is_agency = assignment.is_agency;
    }
    assignments
}

impl crate::data_store::DataStore for AutopilotStore {
    fn key(&self) -> &'static str {
        "autopilots"
    }

    fn export(&self) -> serde_json::Value {
        // Belt AND braces on the Track B1 board-health verdict. `record_run`
        // already strips it before persisting, so a record written by this build
        // carries none — but a record written by an intermediate build (or
        // restored from one) could, and this is the boundary where it would
        // leave the machine. The verdict is derived from the LOCAL
        // `scraping::board_health` store, which is deliberately not a
        // `DataStore`; letting it ride out inside `lastRunSummaries` would
        // replay this machine's failure streaks, error text and run ids on
        // whatever machine imports the bundle.
        let mut records = self.list();
        strip_board_health(records.iter_mut());
        serde_json::json!(records)
    }

    fn import(&self, data: &serde_json::Value) -> AppResult<usize> {
        // Same boundary as `export`, the other direction: a legacy/tampered
        // backup can carry `lastRunSummaries[].health` (the field predates
        // this strip, or a hand-edited bundle), and `replace_all` below
        // persists whatever it's handed verbatim — closing this side of the
        // door is what `export`'s own "belt AND braces" comment describes.
        let mut items: Vec<Autopilot> =
            serde_json::from_value(data.clone()).map_err(|e| e.to_string())?;
        strip_board_health(items.iter_mut());
        let count = items.len();
        self.replace_all(items);
        Ok(count)
    }
}

/// Strip the Track B1 `health` verdict from every record's
/// `last_run_summaries`. It is a DISPLAY-TIME derivation of the live
/// `board_health` store, never persisted run state (see `record_run`'s own
/// strip, right above `last_run_summaries` in this file) — this is the same
/// scrub applied at every boundary that can put non-`record_run`-authored
/// data into memory or onto disk: `export` (leaving the machine), `import` (a
/// legacy/tampered backup coming back in), and `AutopilotStore::load` (an
/// on-disk `autopilots.json` written by an intermediate build before this
/// strip existed, or edited by hand — `load`'s cache means a mutation
/// unrelated to `last_run_summaries` would otherwise re-persist such a
/// record's stale health forever).
fn strip_board_health<'a>(records: impl IntoIterator<Item = &'a mut Autopilot>) {
    for ap in records {
        for summary in &mut ap.last_run_summaries {
            summary.health = None;
        }
    }
}

#[cfg(test)]
mod test;
