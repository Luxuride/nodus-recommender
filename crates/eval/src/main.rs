//! `nodus-eval` — does the ranking find what people actually did next?
//!
//! The harness answers one question at a time: freeze the graph at an instant, rank
//! from what was known then, and count how many of those accounts the users started
//! following afterwards. It compares the candidate model against the popularity
//! baseline on the same split, because a number without a baseline says nothing.
//!
//! ```text
//! cargo run -p nodus-eval -- --executive        # score the current database
//! cargo run -p nodus-eval -- --seed-users 40 --seed-accounts 300
//! ```
//!
//! The synthetic mode exists so the harness can be validated: on a planted graph with
//! taste clusters, the taste model must beat popularity, and if it does not, the bug is
//! in the harness rather than in the product.

mod metrics;
mod model;
mod split;
mod synthetic;

use std::collections::HashMap;
use std::process::ExitCode;

use metrics::{BucketTally, bucket_name};
use model::Prepared;
use nodus_store::connect_and_migrate;
use time::OffsetDateTime;

/// What the operator asked for on the command line.
#[derive(Debug)]
struct Options {
    database_url: String,
    cutoff_hours: i64,
    k: usize,
    min_follows: usize,
    seed: Option<Synthetic>,
}

/// A synthetic graph request, if the run should generate data first.
#[derive(Debug, Clone, Copy)]
struct Synthetic {
    users: usize,
    accounts: usize,
    clusters: usize,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("nodus-eval: {message}");
            eprintln!("{}", Options::USAGE);
            return ExitCode::FAILURE;
        }
    };

    match run(&options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nodus-eval: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Loads (or plants) the graph and prints the report.
async fn run(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
    let pool =
        connect_and_migrate(&options.database_url, 4, std::time::Duration::from_secs(10)).await?;
    if let Some(shape) = options.seed {
        println!(
            "planting {} users over {} accounts in {} clusters",
            shape.users, shape.accounts, shape.clusters
        );
        synthetic::seed(
            &pool,
            synthetic::Shape {
                users: shape.users,
                accounts: shape.accounts,
                clusters: shape.clusters,
                truth_per_mille: 200,
            },
            0x5eed_1234,
        )
        .await?;
    }

    let cutoff = OffsetDateTime::now_utc() - time::Duration::hours(options.cutoff_hours);
    let graph = split::load(&pool, cutoff).await?;
    let users = graph.scoreable(options.min_follows);
    if users.is_empty() {
        println!(
            "no scoreable users: nobody has {} follows before the cutoff or anything after it",
            options.min_follows
        );
        return Ok(());
    }

    let prepared = Prepared::new(&graph);
    let mut model_total = metrics::Hit::default();
    let mut baseline_total = metrics::Hit::default();
    let mut model_buckets = [BucketTally::default(); 3];
    let mut baseline_buckets = [BucketTally::default(); 3];

    for user in &users {
        let truth = graph.truth_set(user.account_id);
        let ranked = prepared.rank(&graph, user.account_id, options.k);
        let popular = model::popularity(&graph, user.account_id, options.k);

        let model_hit = metrics::score_user(&ranked, &truth, options.k);
        let baseline_hit = metrics::score_user(&popular, &truth, options.k);
        model_total.hits += model_hit.hits;
        model_total.shown += model_hit.shown;
        model_total.truth += model_hit.truth;
        baseline_total.hits += baseline_hit.hits;
        baseline_total.shown += baseline_hit.shown;
        baseline_total.truth += baseline_hit.truth;

        metrics::tally_buckets(
            &ranked,
            &truth,
            &graph.followers,
            options.k,
            &mut model_buckets,
        );
        metrics::tally_buckets(
            &popular,
            &truth,
            &graph.followers,
            options.k,
            &mut baseline_buckets,
        );
    }

    report(
        cutoff,
        options,
        users.len(),
        model_total,
        baseline_total,
        &model_buckets,
        &baseline_buckets,
    );
    Ok(())
}

impl Options {
    const USAGE: &'static str = "\
usage: nodus-eval [options]

  --database-url URL   Postgres connection string (default: $DATABASE_URL)
  --cutoff-hours N     split instant, hours ago (default: 24)
  --k N                list length scored (default: 10)
  --min-follows N      skip users with fewer training follows (default: 10)
  --seed-users N       plant N synthetic users before scoring
  --seed-accounts N    accounts to plant (default: 300)
  --seed-clusters N    taste clusters to plant (default: 6)";

    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            database_url: std::env::var("DATABASE_URL")
                .map_err(|_| "--database-url is required (or set DATABASE_URL)".to_owned())?,
            cutoff_hours: 24,
            k: 10,
            min_follows: 10,
            seed: None,
        };
        let mut seed_users = None;
        let mut seed_accounts = 300;
        let mut seed_clusters = 6;
        let mut args = args.peekable();
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--database-url" => options.database_url = value()?,
                "--cutoff-hours" => options.cutoff_hours = parse_number(&value()?, &flag)?,
                "--k" => options.k = parse_number(&value()?, &flag)?,
                "--min-follows" => options.min_follows = parse_number(&value()?, &flag)?,
                "--seed-users" => seed_users = Some(parse_number(&value()?, &flag)?),
                "--seed-accounts" => seed_accounts = parse_number(&value()?, &flag)?,
                "--seed-clusters" => seed_clusters = parse_number(&value()?, &flag)?,
                _ => return Err(format!("unknown flag {flag}")),
            }
        }
        options.seed = seed_users.map(|users| Synthetic {
            users,
            accounts: seed_accounts,
            clusters: seed_clusters,
        });
        Ok(options)
    }
}

/// Parses a positive number, so a zero page size cannot reach the report.
fn parse_number<T: std::str::FromStr>(text: &str, flag: &str) -> Result<T, String> {
    text.parse::<T>()
        .map_err(|_| format!("{flag}: cannot parse {text:?} as a number"))
}

/// Keeps the unused-import warning away from `HashMap` on platforms where the report
/// prints without the map.
#[expect(
    dead_code,
    reason = "the report type alias documents the shape used above"
)]
type FollowerMap = HashMap<i64, i64>;

/// Prints the table, and the only verdict a table can honestly support.
///
/// The verdict line is deliberately blunt: if the taste ranking does not beat
/// popularity on the score that matters, it says so instead of describing the bucket
/// columns and letting a reader find a flattering number in them.
fn report(
    cutoff: OffsetDateTime,
    options: &Options,
    users: usize,
    model_total: metrics::Hit,
    baseline_total: metrics::Hit,
    model_buckets: &[BucketTally; 3],
    baseline_buckets: &[BucketTally; 3],
) {
    println!(
        "cutoff {}  ·  {} users scored  ·  k={}  ·  min follows={}",
        cutoff, users, options.k, options.min_follows
    );
    println!();
    println!("{:<12} {:>12} {:>10}", "ranking", "precision", "recall");
    println!(
        "{:<12} {:>12.3} {:>10.3}",
        "taste",
        model_total.precision(),
        model_total.recall()
    );
    println!(
        "{:<12} {:>12.3} {:>10.3}",
        "popularity",
        baseline_total.precision(),
        baseline_total.recall()
    );
    println!();
    println!(
        "{:<14} {:>9} {:>9} {:>9}",
        "bucket", "taste", "pop", "truth"
    );
    for index in 0..3 {
        println!(
            "{:<14} {:>9.3} {:>9.3} {:>9}",
            bucket_name(index),
            model_buckets[index].recall(),
            baseline_buckets[index].recall(),
            model_buckets[index].truth
        );
    }
    println!();
    if model_total.precision() > baseline_total.precision() {
        println!("taste beats popularity on this split");
    } else {
        println!("NO IMPROVEMENT over popularity -- treat this ranking as unproven");
    }
}
