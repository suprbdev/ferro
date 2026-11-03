use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use queue::{
    Job, NewJob, Queue, QueueError, QueueOptions, Recurring, Runner, RunnerConfig,
};

#[derive(Parser)]
#[command(name = "queue", about = "A durable job queue on ferro")]
struct Cli {
    #[arg(long, default_value = "queue.ferro", global = true)]
    db: PathBuf,
    #[arg(long, default_value_t = 10_000, global = true, help = "How long to wait for another process holding the writer lock")]
    lock_wait_ms: u64,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Args, Clone)]
struct JobOpts {
    #[arg(long, default_value_t = 100)]
    priority: u8,
    #[arg(long, default_value_t = 0)]
    delay_ms: u64,
    #[arg(long, default_value_t = 5)]
    max_attempts: u32,
    #[arg(long, default_value_t = 1000)]
    backoff_ms: u64,
    #[arg(long)]
    key: Option<String>,
    #[arg(long, default_value_t = 86_400_000)]
    key_ttl_ms: u64,
    #[arg(long, value_delimiter = ',')]
    after: Vec<u64>,
}

impl JobOpts {
    fn spec(&self, queue: &str, payload: Vec<u8>) -> NewJob {
        NewJob::new(queue, payload)
            .priority(self.priority)
            .delay_ms(self.delay_ms)
            .max_attempts(self.max_attempts)
            .backoff_ms(self.backoff_ms)
            .key_ttl_ms(self.key_ttl_ms)
            .after(self.after.clone())
    }
}

#[derive(Subcommand)]
enum Cmd {
    #[command(about = "Add one job")]
    Enqueue {
        queue: String,
        payload: String,
        #[command(flatten)]
        opts: JobOpts,
    },
    #[command(about = "Add one job per input line in a single commit")]
    EnqueueBatch {
        queue: String,
        #[arg(long, help = "Read lines from this file instead of stdin")]
        file: Option<PathBuf>,
        #[command(flatten)]
        opts: JobOpts,
    },
    #[command(about = "Enqueue generated jobs in batches, logging each acknowledged id")]
    Produce {
        queue: String,
        #[arg(long)]
        count: u64,
        #[arg(long, default_value_t = 1)]
        batch: u64,
        #[arg(long)]
        log: Option<PathBuf>,
        #[arg(long, default_value_t = 0)]
        sleep_ms: u64,
        #[arg(long, default_value = "p")]
        prefix: String,
    },
    #[command(about = "Lease the highest priority ready jobs")]
    Lease {
        queue: String,
        #[arg(long, default_value = "cli")]
        worker: String,
        #[arg(long, default_value_t = 30_000)]
        ttl_ms: u64,
        #[arg(long, default_value_t = 1)]
        n: usize,
    },
    #[command(about = "Finish a leased job")]
    Ack { id: u64, attempt: u32 },
    #[command(about = "Fail a leased job, retrying with backoff or dead lettering")]
    Nack {
        id: u64,
        attempt: u32,
        #[arg(long, default_value = "nacked")]
        error: String,
        #[arg(long)]
        retry_in_ms: Option<u64>,
    },
    #[command(about = "Push a lease's expiry out")]
    Extend {
        id: u64,
        attempt: u32,
        #[arg(long, default_value_t = 30_000)]
        ttl_ms: u64,
    },
    #[command(about = "Run workers that execute a shell command per job")]
    Work {
        #[arg(required = true)]
        queues: Vec<String>,
        #[arg(long, default_value_t = 1)]
        workers: usize,
        #[arg(long, default_value_t = 1)]
        batch: usize,
        #[arg(long, default_value_t = 30_000)]
        ttl_ms: u64,
        #[arg(long, name = "exec", help = "Shell command; payload on stdin, QUEUE_JOB_ID, QUEUE_NAME and QUEUE_ATTEMPT in env")]
        exec: String,
        #[arg(long)]
        max_jobs: Option<u64>,
        #[arg(long)]
        exit_when_idle: bool,
    },
    #[command(about = "Requeue expired leases, promote due jobs, spawn recurring jobs")]
    Tick,
    #[command(about = "Show queue counts")]
    Stats { queue: Option<String> },
    #[command(about = "Show one live job")]
    Show { id: u64 },
    #[command(about = "Stop leasing from a queue")]
    Pause { queue: String },
    #[command(about = "Allow leasing from a queue again")]
    Resume { queue: String },
    #[command(subcommand, about = "Dead letter queue")]
    Dlq(Dlq),
    #[command(subcommand, about = "Recurring jobs")]
    Recurring(Rec),
    #[command(about = "Every state a job passed through, from the commit log")]
    History { id: u64 },
    #[command(about = "Re-enqueue the jobs that were in flight at a past version")]
    Replay {
        #[arg(long)]
        from: u64,
        #[arg(long)]
        include_pending: bool,
        #[arg(long)]
        force: bool,
    },
    #[command(about = "File size and version")]
    Info,
}

#[derive(Subcommand)]
enum Dlq {
    List {
        #[arg(long)]
        queue: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Retry {
        id: Option<u64>,
        #[arg(long, conflicts_with = "id")]
        queue: Option<String>,
    },
}

#[derive(Subcommand)]
enum Rec {
    Add {
        name: String,
        queue: String,
        payload: String,
        #[arg(long)]
        every_ms: u64,
        #[arg(long, default_value_t = 100)]
        priority: u8,
        #[arg(long, default_value_t = 5)]
        max_attempts: u32,
        #[arg(long, default_value_t = 0)]
        first_in_ms: u64,
    },
    Remove {
        name: String,
    },
    List,
}

fn show_payload(job: &Job) -> String {
    String::from_utf8_lossy(&job.payload).into_owned()
}

fn run(cli: Cli) -> Result<(), QueueError> {
    let options = QueueOptions {
        lock_wait: Duration::from_millis(cli.lock_wait_ms),
        ..QueueOptions::default()
    };
    let open = || Queue::open(&cli.db, options.clone());
    let open_ro = || Queue::open_read_only(&cli.db, options.clone());
    match cli.command {
        Cmd::Enqueue { queue, payload, opts } => {
            let mut spec = opts.spec(&queue, payload.into_bytes());
            spec.key = opts.key.clone();
            let done = open()?.enqueue(spec)?;
            if done.duplicate {
                println!("duplicate job={}", done.id);
            } else {
                println!("enqueued job={}", done.id);
            }
        }
        Cmd::EnqueueBatch { queue, file, opts } => {
            if opts.key.is_some() {
                return Err(QueueError::Invalid("--key applies to a single job".into()));
            }
            let lines: Vec<String> = match file {
                Some(path) => std::io::BufReader::new(std::fs::File::open(path).map_err(ferro::Error::from)?)
                    .lines()
                    .collect::<Result<_, _>>()
                    .map_err(ferro::Error::from)?,
                None => std::io::stdin()
                    .lock()
                    .lines()
                    .collect::<Result<_, _>>()
                    .map_err(ferro::Error::from)?,
            };
            let specs = lines
                .into_iter()
                .filter(|line| !line.is_empty())
                .map(|line| opts.spec(&queue, line.into_bytes()))
                .collect();
            let done = open()?.enqueue_batch(specs)?;
            let ids: Vec<String> = done.iter().map(|d| d.id.to_string()).collect();
            println!("enqueued count={} first={} last={}", done.len(), ids.first().map(String::as_str).unwrap_or("-"), ids.last().map(String::as_str).unwrap_or("-"));
        }
        Cmd::Produce { queue, count, batch, log, sleep_ms, prefix } => {
            let q = open()?;
            let mut log = match log {
                Some(path) => Some(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .map_err(ferro::Error::from)?,
                ),
                None => None,
            };
            let mut made = 0;
            while made < count {
                let take = batch.min(count - made);
                let specs = (0..take)
                    .map(|i| NewJob::new(queue.clone(), format!("{prefix}-{}", made + i).into_bytes()))
                    .collect();
                let done = q.enqueue_batch(specs)?;
                if let Some(file) = log.as_mut() {
                    let text: String = done.iter().map(|d| format!("{}\n", d.id)).collect();
                    file.write_all(text.as_bytes()).map_err(ferro::Error::from)?;
                }
                made += take;
                if sleep_ms > 0 {
                    std::thread::sleep(Duration::from_millis(sleep_ms));
                }
            }
            println!("produced count={made}");
        }
        Cmd::Lease { queue, worker, ttl_ms, n } => {
            let leased = open()?.lease_n(&queue, &worker, ttl_ms, n)?;
            if leased.is_empty() {
                println!("empty");
            }
            for item in leased {
                println!(
                    "leased job={} attempt={} queue={} priority={} payload={}",
                    item.job.id,
                    item.job.attempts,
                    item.job.queue,
                    item.job.priority,
                    show_payload(&item.job)
                );
            }
        }
        Cmd::Ack { id, attempt } => {
            open()?.ack(queue::Lease { id, attempt })?;
            println!("acked job={id}");
        }
        Cmd::Nack { id, attempt, error, retry_in_ms } => {
            let state = open()?.nack(queue::Lease { id, attempt }, &error, retry_in_ms)?;
            println!("nacked job={id} state={state}");
        }
        Cmd::Extend { id, attempt, ttl_ms } => {
            open()?.extend(queue::Lease { id, attempt }, ttl_ms)?;
            println!("extended job={id}");
        }
        Cmd::Work { queues, workers, batch, ttl_ms, exec, max_jobs, exit_when_idle } => {
            let q = open()?;
            let mut config = RunnerConfig::new(queues.clone(), workers);
            config.batch = batch;
            config.ttl_ms = ttl_ms;
            let handler: queue::runner::Handler = Arc::new(move |job: &Job| run_exec(&exec, job));
            let watcher = q.watch();
            let runner = Runner::start(q.clone(), config, handler);
            loop {
                if max_jobs.is_some_and(|max| runner.done() >= max) {
                    break;
                }
                if exit_when_idle {
                    let idle = queues.iter().all(|name| {
                        q.stats(Some(name))
                            .iter()
                            .all(|s| s.ready + s.delayed + s.leased == 0)
                    });
                    if idle {
                        break;
                    }
                }
                let _ = watcher.recv_timeout(Duration::from_millis(250));
            }
            let (done, failed, lost) = runner.stop();
            println!("done={done} failed={failed} lost={lost}");
        }
        Cmd::Tick => {
            let tick = open()?.tick()?;
            println!(
                "tick expired={} promoted={} spawned={} pruned={}",
                tick.expired.len(),
                tick.promoted,
                tick.spawned,
                tick.pruned
            );
        }
        Cmd::Stats { queue } => {
            let all = open_ro()?.stats(queue.as_deref());
            for s in all {
                println!(
                    "queue={} ready={} delayed={} leased={} blocked={} dead={} paused={}",
                    s.queue, s.ready, s.delayed, s.leased, s.blocked, s.dead, s.paused
                );
            }
        }
        Cmd::Show { id } => match open_ro()?.get(id) {
            Some(job) => println!("{job} payload={}", show_payload(&job)),
            None => return Err(QueueError::NotFound(id)),
        },
        Cmd::Pause { queue } => {
            open()?.pause(&queue)?;
            println!("paused queue={queue}");
        }
        Cmd::Resume { queue } => {
            open()?.resume(&queue)?;
            println!("resumed queue={queue}");
        }
        Cmd::Dlq(Dlq::List { queue, limit }) => {
            let jobs = open_ro()?.dlq_list(queue.as_deref(), limit);
            for job in &jobs {
                println!("{job} payload={}", show_payload(job));
            }
            if jobs.is_empty() {
                println!("dead letter queue is empty");
            }
        }
        Cmd::Dlq(Dlq::Retry { id, queue }) => {
            let q = open()?;
            match (id, queue) {
                (Some(id), _) => {
                    q.dlq_retry(id)?;
                    println!("retried count=1");
                }
                (None, Some(queue)) => {
                    let count = q.dlq_retry_queue(&queue)?;
                    println!("retried count={count}");
                }
                (None, None) => {
                    return Err(QueueError::Invalid("give a job id or --queue".into()));
                }
            }
        }
        Cmd::Recurring(Rec::Add { name, queue, payload, every_ms, priority, max_attempts, first_in_ms }) => {
            let q = open()?;
            let next_run = q.now() + first_in_ms;
            q.recurring_add(Recurring {
                name: name.clone(),
                queue,
                priority,
                payload: payload.into_bytes().into(),
                every_ms,
                next_run,
                max_attempts,
            })?;
            println!("recurring added name={name}");
        }
        Cmd::Recurring(Rec::Remove { name }) => {
            let removed = open()?.recurring_remove(&name)?;
            println!("recurring removed={removed} name={name}");
        }
        Cmd::Recurring(Rec::List) => {
            for rec in open_ro()?.recurring_list() {
                println!(
                    "name={} queue={} every_ms={} next_run={} priority={}",
                    rec.name, rec.queue, rec.every_ms, rec.next_run, rec.priority
                );
            }
        }
        Cmd::History { id } => {
            for event in open_ro()?.history(id)? {
                match &event.job {
                    Some(job) => println!(
                        "v{} t={} {:?} | {job}",
                        event.version, event.time_ms, event.message
                    ),
                    None => println!(
                        "v{} t={} {:?} | job={id} finished",
                        event.version, event.time_ms, event.message
                    ),
                }
            }
        }
        Cmd::Replay { from, include_pending, force } => {
            let done = open()?.replay(from, include_pending, force)?;
            for item in &done {
                println!(
                    "replayed job={} as={} duplicate={}",
                    item.from_job, item.new_job, item.duplicate
                );
            }
            println!("replayed count={}", done.len());
        }
        Cmd::Info => {
            let q = open_ro()?;
            println!("version={} file_bytes={}", q.version(), q.size_on_disk());
        }
    }
    Ok(())
}

fn run_exec(exec: &str, job: &Job) -> Result<(), String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(exec)
        .env("QUEUE_JOB_ID", job.id.to_string())
        .env("QUEUE_NAME", &job.queue)
        .env("QUEUE_ATTEMPT", job.attempts.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&job.payload);
    }
    let mut err = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut err);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exit {status}: {}", err.trim()))
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
