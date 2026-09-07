//! Pure decision logic for post-discovery LinkedIn description enrichment
//! (issue #1114, LinkedIn slice). LinkedIn search results never carry a
//! description — `LinkedInApiClient` always builds a posting with
//! `description: Some(String::new())` (no board-side detail call) — so a
//! LinkedIn [`FoundJob`] reaches `record_run` title-only, forever, unless
//! something resolves it after the fact.
//!
//! This is L2 (`autopilot_helpers`): it owns only the pure "which URLs need a
//! fetch" / "what does a fetch outcome mean" decisions, so they stay testable
//! without a live `AppHandle`. The actual fetch/rate-limit/write-back
//! orchestration is Tauri-shaped (an `AppHandle`, a `#[tauri::command]` call)
//! and therefore lives one layer up, in `commands::autopilot::linkedin_enrich`
//! (L3) — see `docs/architecture-rules.md` R2/R7.

use crate::autopilot::FoundJob;
use crate::documents::keywords::description_is_blank;
use crate::scraping::JobPosting;

/// Hard per-run cap on LinkedIn enrichment fetches. Mirrors the reasoning
/// behind `autopilot_helpers::ASSISTANT_NOTES_MAX` — a small, fixed ceiling on
/// an unattended background fan-out. LinkedIn's own process-wide rate limiter
/// (`linkedin_rate_limiter()`, 10 requests/60s default) already paces the
/// fetches themselves; this cap instead bounds the WORST-CASE wall clock this
/// pass can add after a run that surfaces a large batch of new LinkedIn
/// postings at once (e.g. an autopilot paused for days, or a broad search) —
/// at 15 fetches and 10/60s pacing, the pass finishes within ~2 scrapes of the
/// window, never accumulating unbounded background load across runs.
pub(crate) const LINKEDIN_ENRICH_MAX: usize = 15;

/// Board id LinkedIn found-jobs are tagged with (`JobPosting.source` /
/// `FoundJob.board`) — matches the id LinkedIn's own `Scraper::id()` returns.
const LINKEDIN_BOARD_ID: &str = "linkedin";

/// Pure selection: which found-job URLs need a LinkedIn description
/// enrichment fetch this run. Filters on **blank description AND
/// `board == "linkedin"`** — deliberately NOT `is_new` — so a posting whose
/// enrichment attempt failed on a prior run (network error, expired LinkedIn
/// auth, rate limit, selector drift) is retried on the next run instead of
/// being permanently stuck title-only just because it was already "seen".
/// Deduped (a posting can appear more than once across clustered members) and
/// capped at [`LINKEDIN_ENRICH_MAX`].
pub(crate) fn select_linkedin_enrichment_targets(found_jobs: &[FoundJob]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    found_jobs
        .iter()
        .filter(|j| j.board.as_deref() == Some(LINKEDIN_BOARD_ID))
        .filter(|j| description_is_blank(j.description.as_deref()))
        .map(|j| j.url.clone())
        .filter(|url| seen.insert(url.clone()))
        .take(LINKEDIN_ENRICH_MAX)
        .collect()
}

/// A single URL's resolution outcome, classified into what the caller needs
/// to decide. `pub(crate)` so `commands::autopilot::linkedin_enrich` (L3, the
/// only allowed caller of the actual fetch) can match on it; this L2 module
/// itself never touches an `AppHandle` or the network.
#[derive(Debug, PartialEq)]
pub(crate) enum EnrichOutcome {
    /// A genuinely usable (non-blank) description to write back.
    Description(String),
    /// Nothing usable this attempt — a network/HTTP error, a fetch that came
    /// back with no posting (host mismatch, non-2xx, or a genuinely-removed
    /// job — `resolve`'s `Ok(None)` conflates all three, so this pass cannot
    /// tell "gone forever" from "try again"), or a fetch that still returned
    /// a blank description. Every `Skip` case is retried on a later run
    /// because [`select_linkedin_enrichment_targets`] re-selects on blank
    /// description alone, not on any per-attempt state — accepted as a
    /// documented follow-up rather than adding a "gave up" marker: doing this
    /// precisely would need `scrape_url::resolve` to distinguish "removed"
    /// from "transient" upstream, which is a bigger, riskier change to a
    /// function four other callers share.
    Skip,
}

/// Pure decision over one `scraping::scrape_url::resolve` outcome — see
/// [`EnrichOutcome`] for what each case means and why `Ok(None)` and `Err`
/// are not (yet) distinguished. Takes the plain `anyhow::Result` shape
/// (rather than the L1 fn itself) so this stays callable with a fabricated
/// value in a unit test, no network/AppHandle required.
pub(crate) fn classify_resolution(result: anyhow::Result<Option<JobPosting>>) -> EnrichOutcome {
    match result {
        Ok(Some(p)) => match p.description.filter(|d| !description_is_blank(Some(d))) {
            Some(d) => EnrichOutcome::Description(d),
            None => EnrichOutcome::Skip,
        },
        Ok(None) | Err(_) => EnrichOutcome::Skip,
    }
}

#[cfg(test)]
mod test;
