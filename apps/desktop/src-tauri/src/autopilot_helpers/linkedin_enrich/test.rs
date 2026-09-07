use super::*;
use crate::autopilot::ScoreSource;

fn job(board: Option<&str>, description: Option<&str>, url: &str) -> FoundJob {
    FoundJob {
        title: "Engineer".to_string(),
        company: "Acme".to_string(),
        url: url.to_string(),
        location: None,
        board: board.map(str::to_string),
        description: description.map(str::to_string),
        salary_min: None,
        salary_max: None,
        salary_currency: None,
        score: None,
        score_provisional: false,
        score_source: ScoreSource::Keyword,
        found_at: 0,
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

#[test]
fn selects_only_linkedin_jobs_with_a_blank_description() {
    let jobs = vec![
        job(
            Some("linkedin"),
            None,
            "https://www.linkedin.com/jobs/view/1",
        ),
        job(
            Some("linkedin"),
            Some("A real, non-empty job description."),
            "https://www.linkedin.com/jobs/view/2",
        ),
        job(
            Some("greenhouse"),
            None,
            "https://boards.greenhouse.io/acme/jobs/1",
        ),
    ];

    let targets = select_linkedin_enrichment_targets(&jobs);

    assert_eq!(
        targets,
        vec!["https://www.linkedin.com/jobs/view/1".to_string()]
    );
}

#[test]
fn ignores_is_new_entirely_so_a_prior_failed_attempt_retries() {
    // A LinkedIn job that already went through a run and is no longer `is_new`
    // but STILL has a blank description (its last enrichment attempt failed)
    // must be selected again — the filter is blank-description + board, never
    // `is_new`.
    let mut stale = job(
        Some("linkedin"),
        None,
        "https://www.linkedin.com/jobs/view/3",
    );
    stale.is_new = false;

    let targets = select_linkedin_enrichment_targets(std::slice::from_ref(&stale));

    assert_eq!(targets, vec![stale.url]);
}

#[test]
fn dedupes_and_caps_at_the_per_run_limit() {
    let mut jobs: Vec<FoundJob> = (0..(LINKEDIN_ENRICH_MAX + 5))
        .map(|i| {
            job(
                Some("linkedin"),
                None,
                &format!("https://www.linkedin.com/jobs/view/{i}"),
            )
        })
        .collect();
    // A duplicate URL (e.g. a cluster with two members pointing at the same
    // canonical posting) must not consume two slots of the cap.
    jobs.push(job(
        Some("linkedin"),
        None,
        "https://www.linkedin.com/jobs/view/0",
    ));

    let targets = select_linkedin_enrichment_targets(&jobs);

    assert_eq!(targets.len(), LINKEDIN_ENRICH_MAX);
    let unique: std::collections::HashSet<_> = targets.iter().collect();
    assert_eq!(unique.len(), targets.len());
}

#[test]
fn empty_urls_short_circuits_without_touching_the_rate_limiter() {
    // Regression guard for the async pass's early return — asserted at the
    // pure-selection layer since the async fn itself needs a live AppHandle
    // to exercise end-to-end (out of scope for a unit test here; see the
    // scraping-applier-expert handoff note on why this stays a smoke check).
    let jobs: Vec<FoundJob> = Vec::new();
    assert!(select_linkedin_enrichment_targets(&jobs).is_empty());
}

fn posting(description: Option<&str>) -> JobPosting {
    JobPosting {
        id: "linkedin:1".to_string(),
        external_id: Some("1".to_string()),
        title: "Engineer".to_string(),
        company: "Acme".to_string(),
        location: None,
        url: "https://www.linkedin.com/jobs/view/1".to_string(),
        source: "linkedin".to_string(),
        description: description.map(str::to_string),
        requirements: None,
        posted_at: None,
        captured_at: 0,
        extra: std::collections::HashMap::new(),
    }
}

#[test]
fn classify_resolution_extracts_a_real_description() {
    let outcome = classify_resolution(Ok(Some(posting(Some("A real job description.")))));
    assert_eq!(
        outcome,
        EnrichOutcome::Description("A real job description.".to_string())
    );
}

#[test]
fn classify_resolution_skips_a_blank_description() {
    // LinkedIn's own sentinel (`Some("")`) — a resolve that still didn't find
    // real text must not be treated as a usable update.
    assert_eq!(
        classify_resolution(Ok(Some(posting(Some(""))))),
        EnrichOutcome::Skip
    );
    assert_eq!(
        classify_resolution(Ok(Some(posting(None)))),
        EnrichOutcome::Skip
    );
}

#[test]
fn classify_resolution_skips_ok_none_the_same_as_an_error() {
    // Documented as one case, not two (see `EnrichOutcome::Skip`'s doc): a
    // genuinely-removed posting and a transient failure both retry on the
    // next run via `select_linkedin_enrichment_targets` alone.
    assert_eq!(classify_resolution(Ok(None)), EnrichOutcome::Skip);
    assert_eq!(
        classify_resolution(Err(anyhow::anyhow!("network error"))),
        EnrichOutcome::Skip
    );
}
