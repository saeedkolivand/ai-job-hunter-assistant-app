//! Post-discovery description enrichment for LinkedIn-only found jobs (issue
//! #1114, LinkedIn slice). LinkedIn search results never carry a description —
//! `LinkedInApiClient` always builds a posting with `description:
//! Some(String::new())` (no board-side detail call) — so a LinkedIn
//! [`FoundJob`] reaches `record_run` title-only, forever, unless something
//! resolves it after the fact. This module is that something: a background,
//! best-effort pass reusing the SAME single-URL resolver
//! (`scraping::scrape_url::resolve`) the manual "re-check this job" command and
//! the extension's import flow already use — no new scraping logic.

use tauri::AppHandle;

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
/// to decide. Pulled out of the async fetch loop below so this decision is
/// unit-testable without a live `AppHandle`/network.
#[derive(Debug, PartialEq)]
enum EnrichOutcome {
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

/// Pure decision over one [`crate::scraping::scrape_url::resolve`] outcome —
/// see [`EnrichOutcome`] for what each case means and why `Ok(None)` and
/// `Err` are not (yet) distinguished.
fn classify_resolution(result: anyhow::Result<Option<JobPosting>>) -> EnrichOutcome {
    match result {
        Ok(Some(p)) => match p.description.filter(|d| !description_is_blank(Some(d))) {
            Some(d) => EnrichOutcome::Description(d),
            None => EnrichOutcome::Skip,
        },
        Ok(None) | Err(_) => EnrichOutcome::Skip,
    }
}

/// Best-effort background pass: resolve each `url` via the shared single-URL
/// resolver, paced through LinkedIn's own process-wide rate limiter, and write
/// back any real description through the SAME mechanism
/// `scrape_update_description` uses for a manual correction (issue #1109's
/// write-back fix) — so the match scorer and every found-jobs surface pick up
/// the fetched text identically to a user-triggered fix.
///
/// Fire-and-forget: spawned via `tauri::async_runtime::spawn` from
/// `autopilot_run`, right after that run's own `record_run` call (before the
/// "new jobs" notification, though the ordering is moot either way — `spawn`
/// never blocks its caller), so a slow or failing LinkedIn fetch can never
/// delay the run's own "done" state or that notification. A per-URL failure
/// (network error, non-2xx, expired auth, selector drift returning no usable
/// text) is logged and skipped, never propagated — this is a best-effort
/// improvement layered on a scrape that already succeeded, and must never be
/// mistaken for a reason to retry aggressively (LinkedIn-hostile traffic
/// pattern) or to fail the run.
pub(crate) async fn enrich_linkedin_descriptions(app: AppHandle, urls: Vec<String>) {
    if urls.is_empty() {
        return;
    }
    let limiter = crate::scraping::linkedin::rate_limiter::linkedin_rate_limiter();
    let mut enriched = 0usize;
    for url in &urls {
        limiter.wait_for_slot().await;
        let result = crate::scraping::scrape_url::resolve(url).await;
        // Count the attempt against the shared budget regardless of outcome —
        // the request already reached LinkedIn's servers (or was denied one),
        // so a string of failures must still be paced, not retried in a burst.
        limiter.record_request().await;

        if let Err(ref e) = result {
            log::info!(
                "[autopilot] LinkedIn description enrichment failed for one posting: {}",
                crate::observability::sanitize_reason(&e.to_string())
            );
        }
        let EnrichOutcome::Description(description) = classify_resolution(result) else {
            continue;
        };

        // `scrape_update_description` does synchronous file I/O (both stores
        // it patches persist to disk) — never call it inline on this async
        // task; `spawn_blocking` matches every other sync/blocking call this
        // crate makes from async code (e.g. `autopilot_best_matches`).
        let app_for_write = app.clone();
        let url_for_write = url.clone();
        let joined = tauri::async_runtime::spawn_blocking(move || {
            crate::commands::scrape::scrape_update_description(
                app_for_write,
                crate::commands::scrape::ScrapeUpdateDescriptionRequest {
                    url: url_for_write,
                    description,
                },
            )
        })
        .await;
        match joined {
            Ok(Ok(true)) => enriched += 1,
            Ok(Ok(false)) => {
                // The row moved/was dismissed between discovery and this fetch —
                // not an error, just nothing left to enrich.
            }
            Ok(Err(e)) => log::info!(
                "[autopilot] LinkedIn description write-back failed for one posting: {}",
                crate::observability::sanitize_reason(&e.to_string())
            ),
            Err(e) => log::info!(
                "[autopilot] LinkedIn description write-back task failed for one posting: {}",
                crate::observability::sanitize_reason(&e.to_string())
            ),
        }
    }
    if enriched > 0 {
        log::info!(
            "[autopilot] LinkedIn description enrichment: {enriched}/{} postings updated",
            urls.len()
        );
    }
}

#[cfg(test)]
mod test;
