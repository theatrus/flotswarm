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
use flotswarm_types::{ActionConfig, Envelope, ExecPlan, ShaCheck};
use std::path::PathBuf;
use std::time::Duration;

struct Config {
    host: String,
    queue_url: String,
    actions_dir: PathBuf,
    dry_run: bool,
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
        })
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

    loop {
        let resp = sqs
            .receive_message()
            .queue_url(&cfg.queue_url)
            .wait_time_seconds(20)
            .max_number_of_messages(10)
            .send()
            .await
            .context("sqs receive_message")?;

        for msg in resp.messages() {
            // Ok(()) => handled or cleanly-not-for-us → delete.
            // Err     => failure → leave it; SQS redrives → DLQ after N receives.
            match handle(&cfg, msg).await {
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

async fn handle(cfg: &Config, msg: &aws_sdk_sqs::types::Message) -> Result<()> {
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

    for check in &plan.sha_checks {
        verify_sha(check).with_context(|| format!("sha check for arg {:?}", check.arg))?;
    }
    run(&action, &plan).await?;
    eprintln!("done: {} ({})", env.action, env.id);
    Ok(())
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

async fn run(action: &ActionConfig, plan: &ExecPlan) -> Result<()> {
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

    let dur = parse_timeout(action.timeout.as_deref());
    let status = tokio::time::timeout(dur, cmd.status())
        .await
        .map_err(|_| anyhow::anyhow!("action timed out after {dur:?}"))?
        .context("spawning action")?;
    if !status.success() {
        anyhow::bail!("action exited with {status}");
    }
    Ok(())
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
