use std::{collections::BTreeSet, time::Duration};

use sqlx::SqlitePool;
use time::OffsetDateTime;
use tokio::{sync::watch, time::Instant};
use tracing::{warn, Instrument};

use crate::{
    domain::merge_queue::PullRequestNumber,
    error::ErrorCorrelationId,
    lifecycle,
    storage::{
        CiUsageStore, DueRunCost, FinalizedRunCost, JobCostTotals, PullRequestCommitStore,
        RunCostStore, UsageTotals, WorkflowJobLinkStore,
    },
    telemetry::{
        run_cost::{
            bounded_breakdown, breakdown_key, log_run_cost_finalized, PullRequestResolution,
            RunCostTotal, DEFAULT_RUNNER_LABEL_KEY,
        },
        trace::{self, Operation, OperationOutcome, CI_SOURCE_GITHUB_ACTIONS},
        WorkflowTraceEmitter,
    },
};

/// How often the finalizer looks for run attempts whose grace period has elapsed.
pub(crate) const RUN_COST_POLL_INTERVAL: Duration = Duration::from_secs(15);
/// The most run attempts finalized in one poll.
const RUN_COST_BATCH_SIZE: i64 = 100;

/// Totals each completed run attempt once its grace period has elapsed.
///
/// Pending attempts live in SQLite, so an attempt whose grace period spans a restart is finalized
/// by the next process. A failed finalization leaves the attempt pending and pushes its due time
/// back, doubling the delay up to one hour.
#[derive(Clone)]
pub(crate) struct RunCostFinalizer {
    run_costs: RunCostStore,
    job_links: WorkflowJobLinkStore,
    usage: CiUsageStore,
    commits: PullRequestCommitStore,
    emitter: WorkflowTraceEmitter,
}

/// A redacted store failure while finalizing one attempt; the attempt stays pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FinalizeFailure;

/// Wall-clock time advanced by Tokio's monotonic clock.
///
/// Due times are stored as wall-clock timestamps, but advancing Tokio's paused clock in tests
/// must still make them due.
struct FinalizerClock {
    started_at: OffsetDateTime,
    started: Instant,
}

impl FinalizerClock {
    fn start() -> Self {
        Self {
            started_at: OffsetDateTime::now_utc(),
            started: Instant::now(),
        }
    }

    fn now(&self) -> OffsetDateTime {
        self.started_at + self.started.elapsed()
    }
}

impl RunCostFinalizer {
    /// Creates a finalizer over an already migrated SQLite pool.
    pub(crate) fn new(pool: &SqlitePool, emitter: WorkflowTraceEmitter) -> Self {
        Self {
            run_costs: RunCostStore::new(pool.clone()),
            job_links: WorkflowJobLinkStore::new(pool.clone()),
            usage: CiUsageStore::new(pool.clone()),
            commits: PullRequestCommitStore::new(pool.clone()),
            emitter,
        }
    }

    /// Polls for due run attempts every [`RUN_COST_POLL_INTERVAL`] until cancellation.
    ///
    /// The first poll runs at once, so attempts that came due while the process was down are
    /// finalized on startup. Missed polls are skipped.
    pub(crate) async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let clock = FinalizerClock::start();
        let mut ticker = tokio::time::interval(RUN_COST_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = lifecycle::wait_for_cancellation(&mut shutdown) => return,
                _ = ticker.tick() => {
                    self.finalize_due(clock.now(), &shutdown).await;
                }
            }
        }
    }

    /// Finalizes at most one batch of attempts due at or before `now`.
    ///
    /// Shutdown is honored between attempts, never during one.
    ///
    /// # Returns
    ///
    /// The number of attempts this call finalized.
    pub(crate) async fn finalize_due(
        &self,
        now: OffsetDateTime,
        shutdown: &watch::Receiver<bool>,
    ) -> usize {
        let due = match self.run_costs.due(now, RUN_COST_BATCH_SIZE).await {
            Ok(due) => due,
            Err(_) => {
                warn_finalization_failed("due_lookup_failed");
                return 0;
            }
        };
        for _ in 0..due.abandoned {
            warn_finalization_failed("unreadable_abandoned");
        }
        let mut finalized = 0;
        for run in due.runs {
            if *shutdown.borrow() {
                break;
            }
            let span = trace::operation_span(Operation::RunCostFinalize);
            let outcome = self.finalize(&run).instrument(span.clone()).await;
            trace::set_status(
                &span,
                match outcome {
                    Ok(true) => OperationOutcome::Success,
                    Ok(false) => OperationOutcome::NoOp,
                    Err(FinalizeFailure) => OperationOutcome::Failure,
                },
            );
            match outcome {
                Ok(true) => finalized += 1,
                Ok(false) => {}
                Err(FinalizeFailure) => {
                    warn_finalization_failed("finalize_failed");
                    // a failed deferral leaves the attempt due, so the next poll still retries it
                    let _ = self
                        .run_costs
                        .defer(
                            run.repository_id,
                            run.pending.run_id,
                            run.pending.run_attempt,
                            now,
                        )
                        .instrument(span)
                        .await;
                }
            }
        }
        finalized
    }

    async fn finalize(&self, run: &DueRunCost) -> Result<bool, FinalizeFailure> {
        let pending = &run.pending;
        let jobs = self
            .job_links
            .run_totals(run.repository_id, pending.run_id, pending.run_attempt)
            .await
            .map_err(|_| FinalizeFailure)?;
        let usage = self
            .usage
            .run_totals(run.repository_id, pending.run_id, pending.run_attempt)
            .await
            .map_err(|_| FinalizeFailure)?;
        let (pull_request, resolution) = self.resolve_pull_request(run).await?;
        let total = run_cost_total(run, jobs, usage, pull_request, resolution);
        let finalized = self
            .run_costs
            .finalize(
                run.repository_id,
                &FinalizedRunCost {
                    run_id: pending.run_id,
                    run_attempt: pending.run_attempt,
                    pull_request,
                    resolution,
                    total_usd: total.total_usd,
                    price_table_version: total.price_table_version.as_deref(),
                },
            )
            .await
            .map_err(|_| FinalizeFailure)?;
        if finalized {
            self.emitter.emit_run_cost(&total);
            log_run_cost_finalized(&total);
        }
        Ok(finalized)
    }

    /// Uses the pull request the completed delivery resolved, falling back to the commit map.
    async fn resolve_pull_request(
        &self,
        run: &DueRunCost,
    ) -> Result<(Option<PullRequestNumber>, PullRequestResolution), FinalizeFailure> {
        if let Some((number, resolution)) = run.pending.pull_request {
            return Ok((Some(number), resolution));
        }
        let Some(head_sha) = run.pending.head_sha.as_ref() else {
            return Ok((None, PullRequestResolution::None));
        };
        let mapped = self
            .commits
            .get(run.repository_id, head_sha)
            .await
            .map_err(|_| FinalizeFailure)?;
        Ok(match mapped {
            Some(number) => (Some(number), PullRequestResolution::CommitMap),
            None => (None, PullRequestResolution::None),
        })
    }
}

fn run_cost_total(
    run: &DueRunCost,
    jobs: JobCostTotals,
    usage: UsageTotals,
    pull_request: Option<PullRequestNumber>,
    resolution: PullRequestResolution,
) -> RunCostTotal {
    let pending = &run.pending;
    let total_usd = jobs.cost_usd.unwrap_or(0.0)
        + usage
            .cost_by_source
            .iter()
            .map(|(_, cost_usd)| cost_usd)
            .sum::<f64>();
    let cost_by_source = bounded_breakdown(
        jobs.cost_usd
            .map(|cost_usd| (CI_SOURCE_GITHUB_ACTIONS.to_owned(), cost_usd))
            .into_iter()
            .chain(
                usage
                    .cost_by_source
                    .into_iter()
                    .filter_map(|(source, cost_usd)| Some((breakdown_key(&source)?, cost_usd))),
            ),
    );
    let minutes_by_label =
        bounded_breakdown(jobs.minutes_by_label.into_iter().map(|(label, minutes)| {
            (
                label.unwrap_or_else(|| DEFAULT_RUNNER_LABEL_KEY.to_owned()),
                minutes,
            )
        }));
    let versions = jobs
        .price_table_versions
        .into_iter()
        .chain(usage.price_table_versions)
        .collect::<BTreeSet<_>>();
    let price_table_version = (versions.len() == 1)
        .then(|| versions.into_iter().next())
        .flatten();
    RunCostTotal {
        repository_name: run.repository_name.clone(),
        run_id: pending.run_id,
        run_attempt: pending.run_attempt,
        workflow_name: pending.workflow_name.clone(),
        workflow_file: pending.workflow_file.clone(),
        event: pending.event,
        conclusion: pending.conclusion,
        head_sha: pending.head_sha.clone(),
        pull_request,
        resolution,
        total_usd,
        cost_by_source,
        minutes_by_label,
        usage_entries: usage.entries,
        price_table_version,
        finalized_at: std::time::SystemTime::now(),
    }
}

fn warn_finalization_failed(outcome: &'static str) {
    let error_correlation_id = ErrorCorrelationId::new();
    warn!(
        parent: None,
        outcome,
        %error_correlation_id,
        "ci run cost finalization failed"
    );
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use sqlx::SqlitePool;
    use time::OffsetDateTime;
    use tokio::sync::watch;
    use tracing::{instrument::WithSubscriber, Dispatch};
    use tracing_subscriber::fmt::MakeWriter;

    use super::RunCostFinalizer;
    use crate::{
        domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
        storage::{open_database, PendingRunCost, PullRequestCommitStore, RunCostStore},
        telemetry::{
            run_cost::PullRequestResolution,
            trace::CommitSha,
            workflow::{
                DisplayName, WorkflowConclusion, WorkflowEvent, WorkflowRunAttempt, WorkflowRunId,
            },
            WorkflowTraceEmitter,
        },
    };

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const GRACE: time::Duration = time::Duration::seconds(300);

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("captured logs lock is available");
            String::from_utf8(bytes.clone()).expect("captured logs are UTF-8")
        }

        fn count(&self, message: &str) -> usize {
            self.text().matches(message).count()
        }

        fn dispatch(&self) -> Dispatch {
            Dispatch::new(
                tracing_subscriber::fmt()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(self.clone())
                    .finish(),
            )
        }
    }

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("captured logs lock was poisoned"))?
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for CapturedLogs {
        type Writer = CapturedLogWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    async fn database(path: &Path) -> (SqlitePool, RepositoryId) {
        let pool = open_database(path).await.expect("database opens");
        let mut ids = Vec::new();
        for name in ["owner/repo", "owner/other"] {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
                 webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
                 VALUES (?, X'01', X'02', 1, 1, \
                 '2026-11-18T10:00:00.000Z', '2026-11-18T10:00:00.000Z') RETURNING id",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .expect("repository inserts");
            ids.push(id);
        }
        (
            pool,
            RepositoryId::new(ids[0]).expect("repository id is positive"),
        )
    }

    fn pending(
        run_attempt: i64,
        pull_request: Option<(i64, PullRequestResolution)>,
    ) -> PendingRunCost {
        PendingRunCost {
            run_id: WorkflowRunId::new(31).expect("run id is positive"),
            run_attempt: WorkflowRunAttempt::new(run_attempt).expect("attempt is positive"),
            event: WorkflowEvent::PullRequest,
            workflow_name: DisplayName::sanitize("CI"),
            workflow_file: None,
            conclusion: WorkflowConclusion::Cancelled,
            head_sha: CommitSha::parse(HEAD_SHA).ok(),
            pull_request: pull_request.map(|(number, resolution)| {
                (
                    PullRequestNumber::new(number).expect("PR is positive"),
                    resolution,
                )
            }),
        }
    }

    async fn insert_job(
        pool: &SqlitePool,
        repository_id: i64,
        run_attempt: i64,
        job_id: i64,
        cost_usd: f64,
        runner_label: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO workflow_job_links (repository_id, workflow_run_id, workflow_run_attempt, \
             workflow_job_id, conclusion, started_at_nanos, completed_at_nanos, timing_source, \
             updated_at, billable_minutes, cost_usd, price_table_version, runner_label) \
             VALUES (?, 31, ?, ?, 'success', 0, 1, 'reported', '2026-11-18T10:00:00.000Z', \
             2, ?, '0123456789ab', ?)",
        )
        .bind(repository_id)
        .bind(run_attempt)
        .bind(job_id)
        .bind(cost_usd)
        .bind(runner_label)
        .execute(pool)
        .await
        .expect("job link inserts");
    }

    async fn insert_usage(
        pool: &SqlitePool,
        repository_id: i64,
        run_attempt: i64,
        source: &str,
        cost_usd: f64,
    ) {
        sqlx::query(
            "INSERT INTO ci_usage_entries (repository_id, workflow_run_id, workflow_run_attempt, \
             source, cost_usd, price_fell_back, updated_at) \
             VALUES (?, 31, ?, ?, ?, 0, '2026-11-18T10:00:00.000Z')",
        )
        .bind(repository_id)
        .bind(run_attempt)
        .bind(source)
        .bind(cost_usd)
        .execute(pool)
        .await
        .expect("usage entry inserts");
    }

    async fn stored_total(pool: &SqlitePool, run_attempt: i64) -> Option<f64> {
        sqlx::query_scalar(
            "SELECT total_usd FROM run_cost_finalizations \
             WHERE workflow_run_id = 31 AND workflow_run_attempt = ?",
        )
        .bind(run_attempt)
        .fetch_one(pool)
        .await
        .expect("finalization row exists")
    }

    fn running() -> watch::Receiver<bool> {
        watch::channel(false).1
    }

    #[tokio::test]
    async fn a_completed_attempt_is_finalized_once_after_its_grace_period() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let (pool, repository_id) = database(&directory.path().join("exporter.sqlite3")).await;
        RunCostStore::new(pool.clone())
            .schedule(
                repository_id,
                &pending(1, Some((42, PullRequestResolution::PullRequests))),
                OffsetDateTime::now_utc() + GRACE,
            )
            .await
            .expect("attempt schedules");
        let logs = CapturedLogs::default();
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        tokio::time::pause();
        let runner = tokio::spawn(
            RunCostFinalizer::new(&pool, WorkflowTraceEmitter::disabled())
                .run(shutdown_receiver)
                .with_subscriber(logs.dispatch()),
        );

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(290)).await;
        tokio::time::resume();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            stored_total(&pool, 1).await,
            None,
            "still inside the grace period"
        );

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::time::resume();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while stored_total(&pool, 1).await.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the attempt was not finalized after its grace period"
            );
            tokio::task::yield_now().await;
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::time::resume();
        tokio::time::sleep(Duration::from_millis(200)).await;

        shutdown_sender
            .send(true)
            .expect("finalizer receives shutdown");
        runner.await.expect("finalizer joins");
        assert_eq!(logs.count("ci run cost finalized"), 1, "{}", logs.text());
        assert_eq!(logs.count("ci run unattributed"), 0);
    }

    #[tokio::test]
    async fn pending_finalizations_survive_a_restart() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let path = directory.path().join("exporter.sqlite3");
        let (pool, repository_id) = database(&path).await;
        let due_at = OffsetDateTime::now_utc() + GRACE;
        RunCostStore::new(pool.clone())
            .schedule(
                repository_id,
                &pending(1, Some((42, PullRequestResolution::PullRequests))),
                due_at,
            )
            .await
            .expect("attempt schedules");
        insert_job(&pool, repository_id.get(), 1, 81, 0.012, None).await;
        pool.close().await;

        let reopened = open_database(&path).await.expect("database reopens");
        let finalizer = RunCostFinalizer::new(&reopened, WorkflowTraceEmitter::disabled());

        assert_eq!(finalizer.finalize_due(due_at - GRACE, &running()).await, 0);
        assert_eq!(finalizer.finalize_due(due_at, &running()).await, 1);
        assert_eq!(finalizer.finalize_due(due_at + GRACE, &running()).await, 0);
        assert_eq!(stored_total(&reopened, 1).await, Some(0.012));
    }

    #[tokio::test]
    async fn a_failing_attempt_backs_off_instead_of_retrying_every_poll() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let (pool, repository_id) = database(&directory.path().join("exporter.sqlite3")).await;
        let due_at = OffsetDateTime::now_utc();
        RunCostStore::new(pool.clone())
            .schedule(
                repository_id,
                &pending(1, Some((42, PullRequestResolution::PullRequests))),
                due_at,
            )
            .await
            .expect("attempt schedules");
        sqlx::query(
            "CREATE TRIGGER reject_pull_request_totals BEFORE INSERT ON pull_request_run_costs \
             BEGIN SELECT RAISE(ABORT, 'rejected'); END",
        )
        .execute(&pool)
        .await
        .expect("trigger is created");
        let logs = CapturedLogs::default();
        let finalizer = RunCostFinalizer::new(&pool, WorkflowTraceEmitter::disabled());
        let shutdown = running();
        let poll = |now: OffsetDateTime| {
            finalizer
                .finalize_due(now, &shutdown)
                .with_subscriber(logs.dispatch())
        };

        assert_eq!(poll(due_at).await, 0);
        assert_eq!(poll(due_at + time::Duration::seconds(15)).await, 0);
        assert_eq!(
            logs.count("ci run cost finalization failed"),
            1,
            "the next poll skips the deferred attempt"
        );

        sqlx::query("DROP TRIGGER reject_pull_request_totals")
            .execute(&pool)
            .await
            .expect("trigger is dropped");
        assert_eq!(poll(due_at + time::Duration::seconds(30)).await, 1);
        assert_eq!(stored_total(&pool, 1).await, Some(0.0));
    }

    #[tokio::test]
    async fn totals_count_only_the_attempts_own_jobs_and_usage() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let (pool, repository_id) = database(&directory.path().join("exporter.sqlite3")).await;
        let other_repository = repository_id.get() + 1;
        let due_at = OffsetDateTime::now_utc();
        for attempt in [1, 2] {
            RunCostStore::new(pool.clone())
                .schedule(
                    repository_id,
                    &pending(attempt, Some((42, PullRequestResolution::RunName))),
                    due_at,
                )
                .await
                .expect("attempt schedules");
        }
        insert_job(&pool, repository_id.get(), 2, 81, 0.5, Some("ubuntu_xl")).await;
        insert_job(&pool, repository_id.get(), 2, 82, 0.25, None).await;
        insert_usage(&pool, repository_id.get(), 2, "anthropic", 0.125).await;
        insert_usage(&pool, repository_id.get(), 2, "open-router", 0.0625).await;
        insert_job(&pool, repository_id.get(), 1, 83, 4.0, None).await;
        insert_usage(&pool, repository_id.get(), 1, "anthropic", 8.0).await;
        insert_job(&pool, other_repository, 2, 84, 16.0, None).await;
        insert_usage(&pool, other_repository, 2, "anthropic", 32.0).await;
        let logs = CapturedLogs::default();

        let finalized = RunCostFinalizer::new(&pool, WorkflowTraceEmitter::disabled())
            .finalize_due(due_at, &running())
            .with_subscriber(logs.dispatch())
            .await;

        assert_eq!(finalized, 2, "each attempt is finalized on its own");
        assert_eq!(stored_total(&pool, 2).await, Some(0.9375));
        assert_eq!(stored_total(&pool, 1).await, Some(12.0));
        let text = logs.text();
        assert!(text.contains("ci.cost.total_usd=0.9375"), "{text}");
        assert!(
            text.contains(
                "ci.cost.by_source=\"anthropic=0.125,github_actions=0.75,open_router=0.0625\""
            ),
            "{text}"
        );
        assert!(
            text.contains("ci.minutes.by_label=\"default=2,ubuntu_xl=2\""),
            "{text}"
        );
        assert!(text.contains("ci.usage.entries=2"), "{text}");
        assert!(
            text.contains("cicd.pipeline.result=\"cancellation\""),
            "{text}"
        );
        assert!(text.contains("github.pr_resolution=\"run_name\""), "{text}");
        assert!(
            text.contains("ci.price_table.version=\"0123456789ab\""),
            "{text}"
        );
        let pull_request_totals: Vec<(i64, i64, f64)> = sqlx::query_as(
            "SELECT pull_request_number, workflow_run_attempt, total_usd \
             FROM pull_request_run_costs ORDER BY workflow_run_attempt",
        )
        .fetch_all(&pool)
        .await
        .expect("pull-request totals load");
        assert_eq!(pull_request_totals, [(42, 1, 12.0), (42, 2, 0.9375)]);
    }

    #[tokio::test]
    async fn the_commit_map_attributes_a_run_whose_payload_names_no_pull_request() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let (pool, repository_id) = database(&directory.path().join("exporter.sqlite3")).await;
        let due_at = OffsetDateTime::now_utc();
        PullRequestCommitStore::new(pool.clone())
            .record(
                repository_id,
                &CommitSha::parse(HEAD_SHA).expect("sha parses"),
                PullRequestNumber::new(77).expect("PR is positive"),
            )
            .await
            .expect("commit maps");
        RunCostStore::new(pool.clone())
            .schedule(repository_id, &pending(1, None), due_at)
            .await
            .expect("attempt schedules");
        let logs = CapturedLogs::default();

        RunCostFinalizer::new(&pool, WorkflowTraceEmitter::disabled())
            .finalize_due(due_at, &running())
            .with_subscriber(logs.dispatch())
            .await;

        let text = logs.text();
        assert!(
            text.contains("github.pr_resolution=\"commit_map\""),
            "{text}"
        );
        assert!(text.contains("github.pull_request.number=77"), "{text}");
        let stored: (i64, String) = sqlx::query_as(
            "SELECT pull_request_number, pull_request_resolution FROM run_cost_finalizations",
        )
        .fetch_one(&pool)
        .await
        .expect("finalization loads");
        assert_eq!(stored, (77, "commit_map".to_owned()));
    }

    #[tokio::test]
    async fn an_unattributed_run_still_emits_its_total_and_warns() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let (pool, repository_id) = database(&directory.path().join("exporter.sqlite3")).await;
        let due_at = OffsetDateTime::now_utc();
        RunCostStore::new(pool.clone())
            .schedule(repository_id, &pending(1, None), due_at)
            .await
            .expect("attempt schedules");
        insert_usage(&pool, repository_id.get(), 1, "anthropic", 0.5).await;
        let logs = CapturedLogs::default();

        RunCostFinalizer::new(&pool, WorkflowTraceEmitter::disabled())
            .finalize_due(due_at, &running())
            .with_subscriber(logs.dispatch())
            .await;

        assert_eq!(logs.count("ci run cost finalized"), 1);
        assert_eq!(logs.count("ci run unattributed"), 1);
        assert!(logs.text().contains("github.pr_resolution=\"none\""));
        assert_eq!(stored_total(&pool, 1).await, Some(0.5));
        let pull_request_totals: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pull_request_run_costs")
                .fetch_one(&pool)
                .await
                .expect("pull-request totals count");
        assert_eq!(pull_request_totals, 0);
    }

    #[tokio::test]
    async fn shutdown_stops_finalization_before_the_next_attempt() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let (pool, repository_id) = database(&directory.path().join("exporter.sqlite3")).await;
        let due_at = OffsetDateTime::now_utc();
        RunCostStore::new(pool.clone())
            .schedule(repository_id, &pending(1, None), due_at)
            .await
            .expect("attempt schedules");
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        shutdown_sender.send_replace(true);

        let finalized = RunCostFinalizer::new(&pool, WorkflowTraceEmitter::disabled())
            .finalize_due(due_at, &shutdown_receiver)
            .await;

        assert_eq!(finalized, 0);
        assert_eq!(
            stored_total(&pool, 1).await,
            None,
            "the attempt stays pending"
        );
    }
}
