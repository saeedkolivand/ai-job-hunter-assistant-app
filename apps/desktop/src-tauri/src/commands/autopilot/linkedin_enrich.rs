//! Post-discovery LinkedIn description enrichment — the Tauri/orchestration
//! half of issue #1114 (LinkedIn slice). The pure "which URLs need a fetch" /
//! "what does a fetch outcome mean" decisions live one layer down, in
//! `autopilot_helpers::linkedin_enrich` (L2) — this file is L3 (`commands/`)
//! because it holds an `AppHandle`, calls the `scrape_update_description`
//! command, and drives the fetch loop; see that module's doc and
//! `docs/architecture-rules.md` R2/R7 for why the split sits here.

use tauri::AppHandle;

use crate::autopilot_helpers::linkedin_enrich::{classify_resolution, EnrichOutcome};

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
pub(super) async fn enrich_linkedin_descriptions(app: AppHandle, urls: Vec<String>) {
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
