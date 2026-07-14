//! flotswarm-agent — per-host SQS consumer.
//!
//! Long-polls this host's queue; for each message: check it targets us, look up
//! the action in the local allowlist (`/etc/flotswarm/actions.d/`), strictly
//! validate its args, run any git repo-state checks, then exec the mapped command
//! (argv-only, via `sudo -u` when `run_as` is set). Delete on success or on a
//! clean ignore; leave failures for SQS redrive → DLQ.
//!
//! Crypto note: the AWS clients are built over an explicitly aws-lc-rs rustls
//! provider (no `ring`) — see the `Builder::new().tls_provider(...)` call in `main`.
//!
//! Env: `FLOTSWARM_HOST`, `FLOTSWARM_QUEUE_URL` (required); `FLOTSWARM_ACTIONS_DIR`
//! (default `/etc/flotswarm/actions.d`); `FLOTSWARM_DRY_RUN` (validate + report,
//! never exec). Pass `--once` to drain one receive and exit (handy for testing).

use anyhow::{Context, Result};
use flotswarm_types::{ActionConfig, Envelope, ExecPlan, NotifyEvent, ShaCheck};
use std::path::PathBuf;
use std::time::Duration;

struct Config {
    host: String,
    queue_url: String,
    actions_dir: PathBuf,
    dry_run: bool,
    /// Optional SNS topic for outcome notifications. Unset → notifications off.
    sns_topic_arn: Option<String>,
}

impl Config {
    fn from_env() -> Result<Self> {
        Ok(Self {
            host: env_req("FLOTSWARM_HOST")?,
            queue_url: env_req("FLOTSWARM_QUEUE_URL")?,
            actions_dir: std::env::var("FLOTSWARM_ACTIONS_DIR")
                .unwrap_or_else(|_| "/etc/flotswarm/actions.d".to_string())
                .into(),
            dry_run: std::env::var("FLOTSWARM_DRY_RUN").is_ok_and(|v| !v.is_empty() && v != "0"),
            sns_topic_arn: std::env::var("FLOTSWARM_SNS_TOPIC_ARN")
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }
}

/// Outcome notifier — publishes to the SNS bus if a topic is configured; a
/// no-op otherwise (hosts without `FLOTSWARM_SNS_TOPIC_ARN` simply don't notify).
struct Notifier {
    sns: Option<aws_sdk_sns::Client>,
    topic_arn: Option<String>,
    host: String,
}

impl Notifier {
    /// Publish an already-built event; best-effort (a notify failure must never
    /// change the action's success/failure or the SQS delete decision).
    async fn publish(&self, event: &flotswarm_types::NotifyEvent) {
        let (Some(sns), Some(arn)) = (&self.sns, &self.topic_arn) else {
            return;
        };
        let msg = match serde_json::to_string(event) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("notify: serialize failed: {e}");
                return;
            }
        };
        if let Err(e) = sns.publish().topic_arn(arn).message(msg).send().await {
            eprintln!("notify: sns publish failed (non-fatal): {e}");
        }
    }
}

fn env_req(k: &str) -> Result<String> {
    std::env::var(k).with_context(|| format!("missing required env {k}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    let once = std::env::args().any(|a| a == "--once");
    eprintln!(
        "flotswarm-agent: host={} dry_run={} once={}\n  queue={}",
        cfg.host, cfg.dry_run, once, cfg.queue_url
    );

    // AWS HTTP client whose rustls crypto provider is aws-lc-rs (never ring).
    use aws_smithy_http_client::{tls, Builder};
    let http = Builder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::AwsLc,
        ))
        .build_https();
    let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .http_client(http)
        .load()
        .await;
    let sqs = aws_sdk_sqs::Client::new(&shared);
    let notifier = Notifier {
        sns: cfg.sns_topic_arn.as_ref().map(|_| aws_sdk_sns::Client::new(&shared)),
        topic_arn: cfg.sns_topic_arn.clone(),
        host: cfg.host.clone(),
    };

    loop {
        let resp = sqs
            .receive_message()
            .queue_url(&cfg.queue_url)
            .wait_time_seconds(20)
            .max_number_of_messages(10)
            // ApproximateReceiveCount lets the outcome note which retry this is.
            .message_system_attribute_names(aws_sdk_sqs::types::MessageSystemAttributeName::ApproximateReceiveCount)
            .send()
            .await
            .context("sqs receive_message")?;

        for msg in resp.messages() {
            // Ok(()) => handled or cleanly-not-for-us → delete.
            // Err     => failure → leave it; SQS redrives → DLQ after N receives.
            match handle(&cfg, &notifier, msg).await {
                Ok(()) => {
                    if let Some(rh) = msg.receipt_handle() {
                        sqs.delete_message()
                            .queue_url(&cfg.queue_url)
                            .receipt_handle(rh)
                            .send()
                            .await
                            .context("sqs delete_message")?;
                    }
                }
                Err(e) => eprintln!("message failed (→ DLQ after retries): {e:#}"),
            }
        }

        if once {
            break;
        }
    }
    Ok(())
}

fn receive_count(msg: &aws_sdk_sqs::types::Message) -> u32 {
    msg.attributes()
        .and_then(|a| a.get(&aws_sdk_sqs::types::MessageSystemAttributeName::ApproximateReceiveCount))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

async fn handle(
    cfg: &Config,
    notifier: &Notifier,
    msg: &aws_sdk_sqs::types::Message,
) -> Result<()> {
    let body = msg.body().context("message has no body")?;
    let env: Envelope = serde_json::from_str(body).context("parse envelope")?;

    if !env.targets(&cfg.host) {
        eprintln!(
            "skip {}: target {:?} is not {}",
            env.id, env.target, cfg.host
        );
        return Ok(()); // cleanly not for us
    }

    let path = cfg.actions_dir.join(format!("{}.toml", env.action));
    if !path.exists() {
        eprintln!(
            "allowlist miss: no action {:?} ({})",
            env.action,
            path.display()
        );
        return Ok(()); // clean — not a failure
    }

    let toml =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let action =
        ActionConfig::from_toml_str(&toml).with_context(|| format!("parse {}", path.display()))?;
    let plan = action
        .plan(&env)
        .context("validate message args against action schema")?;

    if cfg.dry_run {
        println!(
            "[dry-run] {} → {:?} (run_as {:?}, {} sha-check(s))",
            env.action,
            plan.argv,
            action.run_as,
            plan.sha_checks.len()
        );
        return Ok(());
    }

    // Execution phase: sha checks then run, capturing output. A failure here is
    // the *action's* outcome (notified below) — distinct from the pre-flight
    // parse/target/allowlist checks above, which are silent.
    let started = std::time::Instant::now();
    let exec: Result<RunResult> = async {
        for check in &plan.sha_checks {
            verify_sha(check).with_context(|| format!("sha check for arg {:?}", check.arg))?;
        }
        run(&action, &plan).await
    }
    .await;
    let duration_s = started.elapsed().as_secs_f64();

    let (ok, exit_code, tail) = match &exec {
        Ok(r) => (r.ok, r.exit_code, tail_of(&r.output, 15)),
        Err(e) => (false, None, format!("{e:#}")),
    };

    if action.notify.should_emit(ok) {
        let attempt = receive_count(msg);
        let tail = if attempt > 1 {
            format!("[attempt {attempt}]\n{tail}")
        } else {
            tail
        };
        notifier
            .publish(&NotifyEvent::Outcome {
                host: notifier.host.clone(),
                action: env.action.clone(),
                source: env.source.clone(),
                id: env.id.clone(),
                ok,
                exit_code,
                duration_s,
                tail,
            })
            .await;
    }

    if ok {
        eprintln!("done: {} ({})", env.action, env.id);
        Ok(())
    } else {
        // Return Err so SQS redrives → DLQ; the notification already went out.
        Err(exec
            .err()
            .unwrap_or_else(|| anyhow::anyhow!("action {} exited non-zero", env.action)))
    }
}

/// Last `n` lines of `s`, trimmed of trailing whitespace.
fn tail_of(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.trim_end().lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// `git -C <repo> merge-base --is-ancestor <sha> <ancestor_of>` — the repo-state
/// gate: the commit must exist and be an ancestor of the allowed ref.
fn verify_sha(c: &ShaCheck) -> Result<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&c.repo)
        .args(["merge-base", "--is-ancestor"])
        .arg(&c.value)
        .arg(&c.ancestor_of)
        .status()
        .with_context(|| format!("running git in {}", c.repo))?;
    if !status.success() {
        anyhow::bail!(
            "{} is not an ancestor of {} in {}",
            c.value,
            c.ancestor_of,
            c.repo
        );
    }
    Ok(())
}

/// Result of running an action's command: exit status + captured combined output.
struct RunResult {
    ok: bool,
    exit_code: Option<i32>,
    output: String,
}

async fn run(action: &ActionConfig, plan: &ExecPlan) -> Result<RunResult> {
    let (program, rest) = plan.argv.split_first().expect("plan.argv is non-empty");

    // argv-only: no shell is ever involved, so values can't inject commands.
    let mut cmd = match &action.run_as {
        Some(user) => {
            let mut c = tokio::process::Command::new("sudo");
            c.arg("-u").arg(user).arg("--").arg(program).args(rest);
            c
        }
        None => {
            let mut c = tokio::process::Command::new(program);
            c.args(rest);
            c
        }
    };
    if let Some(wd) = &action.working_dir {
        cmd.current_dir(wd);
    }

    // Capture combined output so a tail can ride the outcome notification; still
    // echo it so the agent journal keeps the full log.
    let dur = parse_timeout(action.timeout.as_deref());
    let out = tokio::time::timeout(dur, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("action timed out after {dur:?}"))?
        .context("spawning action")?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    if !combined.is_empty() {
        eprint!("{combined}");
    }
    Ok(RunResult {
        ok: out.status.success(),
        exit_code: out.status.code(),
        output: combined,
    })
}

/// Parse `"5m"`/`"30s"`/`"1h"`/bare-seconds into a Duration; default 10 minutes.
fn parse_timeout(s: Option<&str>) -> Duration {
    let s = match s {
        Some(s) => s.trim(),
        None => return Duration::from_secs(600),
    };
    let (num, mult) = if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else {
        (s, 1)
    };
    num.trim()
        .parse::<u64>()
        .map(|n| Duration::from_secs(n * mult))
        .unwrap_or(Duration::from_secs(600))
}

#[cfg(test)]
mod tests {
    use super::parse_timeout;
    use std::time::Duration;

    #[test]
    fn timeouts() {
        assert_eq!(parse_timeout(Some("30s")), Duration::from_secs(30));
        assert_eq!(parse_timeout(Some("5m")), Duration::from_secs(300));
        assert_eq!(parse_timeout(Some("1h")), Duration::from_secs(3600));
        assert_eq!(parse_timeout(Some("45")), Duration::from_secs(45));
        assert_eq!(parse_timeout(None), Duration::from_secs(600));
        assert_eq!(parse_timeout(Some("garbage")), Duration::from_secs(600));
    }
}
