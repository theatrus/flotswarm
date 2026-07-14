//! flotswarm-distributor — API Gateway → Lambda ingress.
//!
//! `POST /hooks/{id}`:
//!   1. Load the hook config from SSM `/flotswarm/hooks/{id}` (SecureString JSON:
//!      `{secret, action, target?, branch?, trigger?, forward_sha?}`).
//!   2. Verify GitHub's `X-Hub-Signature-256` (HMAC-SHA256 over the raw body).
//!   3. Apply the hook's trigger: `push` (branch-filtered), `release` (published
//!      release), or `any` (any signed request — e.g. a CI job's own POST).
//!   4. Build a `flotswarm_types::Envelope` and `SendMessage` it to every host
//!      queue in the routing table (SSM `/flotswarm/routing`). No SNS.
//!
//! The security-critical decision is factored into the pure [`evaluate`] fn so it
//! can be unit-tested without AWS. `main`/`dispatch` do the IO around it.
//!
//! Crypto: AWS SDK clients use an explicitly aws-lc-rs rustls provider (no ring);
//! the GitHub HMAC check uses RustCrypto `hmac`/`sha2` (pure Rust, also no ring).

use flotswarm_types::Envelope;
use hmac::{Hmac, Mac};
use lambda_http::{service_fn, Body, Error, Request, RequestExt, Response};
use serde::Deserialize;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::Arc;

type HmacSha256 = Hmac<Sha256>;

/// A hook definition, stored as an SSM SecureString at `/flotswarm/hooks/<id>`.
#[derive(Debug, Deserialize)]
struct HookConfig {
    secret: String,
    action: String,
    #[serde(default = "default_target")]
    target: String,
    #[serde(default = "default_branch")]
    branch: String,
    /// What event fires this hook. `push` (default) branch-filters; `release`
    /// fires on a published GitHub release (right for release-artifact deploys
    /// like site); `any` fires on any signed request (e.g. a CI job
    /// POSTing after it publishes an artifact).
    #[serde(default)]
    trigger: Trigger,
    /// When true, forward the pushed head commit as an `sha` arg (push only).
    #[serde(default)]
    forward_sha: bool,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Trigger {
    #[default]
    Push,
    Release,
    Any,
}

fn default_target() -> String {
    "all".to_string()
}
fn default_branch() -> String {
    "main".to_string()
}

/// The subset of a GitHub push payload we care about.
#[derive(Debug, Deserialize)]
struct GithubPush {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    after: Option<String>,
    repository: Option<Repository>,
}

/// The subset of a GitHub release payload we care about.
#[derive(Debug, Deserialize)]
struct GithubRelease {
    action: Option<String>,
    repository: Option<Repository>,
}

#[derive(Debug, Deserialize)]
struct Repository {
    full_name: Option<String>,
}

fn source_of(repo: Option<Repository>) -> Option<String> {
    repo.and_then(|r| r.full_name)
        .map(|n| format!("github:{n}"))
}

/// The parts of an [`Envelope`] derivable purely from the request (no uuid/clock).
#[derive(Debug, PartialEq, Eq)]
struct Dispatch {
    action: String,
    target: String,
    args: BTreeMap<String, String>,
    source: Option<String>,
}

/// What the request should result in — the pure decision, sans IO.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// GitHub webhook `ping` — acknowledge, do nothing.
    Pong,
    /// Signature missing or wrong — reject.
    Unauthorized,
    /// Valid but nothing to do (non-push event, or branch didn't match).
    Ignored(&'static str),
    /// Verified & matched — dispatch this to the fleet.
    Dispatch(Dispatch),
}

/// Constant-time verification of `X-Hub-Signature-256: sha256=<hex>`.
fn verify_sig(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hex_digest) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_digest) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// The pure ingress decision: given the hook config, the GitHub event name, the
/// signature header, and the raw body, decide what to do. No IO, no clock.
fn evaluate(
    hook: &HookConfig,
    event: Option<&str>,
    sig: Option<&str>,
    body: &[u8],
) -> Result<Outcome, serde_json::Error> {
    if event == Some("ping") {
        return Ok(Outcome::Pong);
    }
    // Everything else must be signed.
    match sig {
        Some(s) if verify_sig(&hook.secret, body, s) => {}
        _ => return Ok(Outcome::Unauthorized),
    }

    let dispatch = |args, source| {
        Ok(Outcome::Dispatch(Dispatch {
            action: hook.action.clone(),
            target: hook.target.clone(),
            args,
            source,
        }))
    };

    match hook.trigger {
        Trigger::Push => {
            if event != Some("push") {
                return Ok(Outcome::Ignored("non-push event"));
            }
            let push: GithubPush = serde_json::from_slice(body)?;
            let want_ref = format!("refs/heads/{}", hook.branch);
            if push.git_ref.as_deref() != Some(want_ref.as_str()) {
                return Ok(Outcome::Ignored("branch not matched"));
            }
            let mut args = BTreeMap::new();
            if hook.forward_sha {
                if let Some(after) = push.after.filter(|s| !s.is_empty()) {
                    args.insert("sha".to_string(), after);
                }
            }
            dispatch(args, source_of(push.repository))
        }
        Trigger::Release => {
            if event != Some("release") {
                return Ok(Outcome::Ignored("non-release event"));
            }
            let rel: GithubRelease = serde_json::from_slice(body)?;
            // A new build is available on create/publish/(pre)release, and — for a
            // rolling "latest" release that CI re-uploads assets to — on `edited`.
            // The deploy is idempotent, so the burst of edits collapses to one
            // real deploy. Only `deleted`/`unpublished` are ignored.
            match rel.action.as_deref() {
                Some("created") | Some("published") | Some("released") | Some("prereleased")
                | Some("edited") => {}
                _ => return Ok(Outcome::Ignored("release action ignored")),
            }
            dispatch(BTreeMap::new(), source_of(rel.repository))
        }
        Trigger::Any => {
            // Any signed request fires — e.g. a CI job POSTing after it publishes
            // an artifact. Best-effort source if the body carries a repository.
            let source = serde_json::from_slice::<GithubRelease>(body)
                .ok()
                .and_then(|r| source_of(r.repository));
            dispatch(BTreeMap::new(), source)
        }
    }
}

struct Ctx {
    ssm: aws_sdk_ssm::Client,
    sqs: aws_sdk_sqs::Client,
    sns: aws_sdk_sns::Client,
    /// Optional notify bus; unset (no `FLOTSWARM_SNS_TOPIC_ARN`) → no notify.
    topic_arn: Option<String>,
}

impl Ctx {
    /// Best-effort notify publish — never affects the HTTP response.
    async fn notify(&self, event: &flotswarm_types::NotifyEvent) {
        let Some(arn) = &self.topic_arn else { return };
        if let Ok(msg) = serde_json::to_string(event) {
            if let Err(e) = self.sns.publish().topic_arn(arn).message(msg).send().await {
                eprintln!("notify: sns publish failed (non-fatal): {e}");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // AWS HTTP client with rustls crypto pinned to aws-lc-rs (never ring).
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
    let ctx = Arc::new(Ctx {
        ssm: aws_sdk_ssm::Client::new(&shared),
        sqs: aws_sdk_sqs::Client::new(&shared),
        sns: aws_sdk_sns::Client::new(&shared),
        topic_arn: std::env::var("FLOTSWARM_SNS_TOPIC_ARN")
            .ok()
            .filter(|s| !s.is_empty()),
    });

    lambda_http::run(service_fn(move |req: Request| {
        let ctx = ctx.clone();
        async move { dispatch(&ctx, req).await }
    }))
    .await
}

async fn dispatch(ctx: &Ctx, req: Request) -> Result<Response<Body>, Error> {
    let hook_id = match req
        .path_parameters()
        .first("id")
        .map(str::to_owned)
        .or_else(|| {
            // Fallback: last path segment after /hooks/.
            req.uri()
                .path()
                .strip_prefix("/hooks/")
                .map(|s| s.trim_end_matches('/').to_owned())
                .filter(|s| !s.is_empty())
        }) {
        Some(id) => id,
        None => return resp(400, "missing hook id"),
    };

    let event = req
        .headers()
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let sig = req
        .headers()
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = req.body().as_ref().to_vec();

    let hook = match load_hook(ctx, &hook_id).await? {
        Some(h) => h,
        None => return resp(404, "unknown hook"),
    };

    match evaluate(&hook, event.as_deref(), sig.as_deref(), &body) {
        Ok(Outcome::Pong) => resp(200, "pong"),
        Ok(Outcome::Unauthorized) => resp(401, "bad signature"),
        Ok(Outcome::Ignored(why)) => resp(200, why),
        Ok(Outcome::Dispatch(d)) => {
            let env = Envelope {
                id: uuid::Uuid::new_v4().to_string(),
                action: d.action,
                target: d.target,
                args: d.args,
                source: d.source,
                ts: now_rfc3339(),
            };
            let n = broadcast(ctx, &env).await?;
            ctx.notify(&flotswarm_types::NotifyEvent::Dispatch {
                hook: hook_id.clone(),
                action: env.action.clone(),
                targets: vec![env.target.clone()],
                source: env.source.clone(),
                outcome: format!("dispatched ({n} queue(s))"),
            })
            .await;
            resp(202, &format!("dispatched {} to {n} queue(s)", env.action))
        }
        Err(e) => resp(400, &format!("bad payload: {e}")),
    }
}

async fn load_hook(ctx: &Ctx, id: &str) -> Result<Option<HookConfig>, Error> {
    let name = format!("/flotswarm/hooks/{id}");
    let out = ctx
        .ssm
        .get_parameter()
        .name(&name)
        .with_decryption(true)
        .send()
        .await;
    match out {
        Ok(o) => {
            let raw = o.parameter().and_then(|p| p.value()).unwrap_or_default();
            Ok(Some(serde_json::from_str(raw).map_err(|e| {
                Error::from(format!("hook {id} config is not valid JSON: {e}"))
            })?))
        }
        Err(e)
            if e.as_service_error()
                .is_some_and(|se| se.is_parameter_not_found()) =>
        {
            Ok(None)
        }
        Err(e) => Err(Error::from(e)),
    }
}

/// Read the routing table and SendMessage the envelope to every host queue.
async fn broadcast(ctx: &Ctx, env: &Envelope) -> Result<usize, Error> {
    #[derive(Deserialize)]
    struct Routing {
        queues: BTreeMap<String, String>,
    }
    let raw = ctx
        .ssm
        .get_parameter()
        .name("/flotswarm/routing")
        .send()
        .await?
        .parameter()
        .and_then(|p| p.value())
        .map(str::to_owned)
        .ok_or_else(|| Error::from("routing table missing"))?;
    let routing: Routing = serde_json::from_str(&raw).map_err(|e| Error::from(e.to_string()))?;

    let body = serde_json::to_string(env)?;
    let mut n = 0;
    for (host, url) in &routing.queues {
        ctx.sqs
            .send_message()
            .queue_url(url)
            .message_body(&body)
            .send()
            .await
            .map_err(|e| Error::from(format!("send to {host} ({url}): {e}")))?;
        n += 1;
    }
    Ok(n)
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn resp(status: u16, msg: &str) -> Result<Response<Body>, Error> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Body::from(format!("{msg}\n")))
        .expect("valid response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn hook() -> HookConfig {
        HookConfig {
            secret: "s3cr3t".into(),
            action: "deploy-site".into(),
            target: "host1".into(),
            branch: "main".into(),
            trigger: Trigger::Push,
            forward_sha: false,
        }
    }

    const PUSH_MAIN: &[u8] =
        br#"{"ref":"refs/heads/main","after":"0123abcdef","repository":{"full_name":"acme/site"}}"#;

    #[test]
    fn ping_needs_no_signature() {
        assert_eq!(
            evaluate(&hook(), Some("ping"), None, b"{}").unwrap(),
            Outcome::Pong
        );
    }

    #[test]
    fn valid_push_dispatches() {
        let sig = sign("s3cr3t", PUSH_MAIN);
        let out = evaluate(&hook(), Some("push"), Some(&sig), PUSH_MAIN).unwrap();
        assert_eq!(
            out,
            Outcome::Dispatch(Dispatch {
                action: "deploy-site".into(),
                target: "host1".into(),
                args: BTreeMap::new(),
                source: Some("github:acme/site".into()),
            })
        );
    }

    #[test]
    fn bad_signature_is_unauthorized() {
        let bad = sign("wrong-key", PUSH_MAIN);
        assert_eq!(
            evaluate(&hook(), Some("push"), Some(&bad), PUSH_MAIN).unwrap(),
            Outcome::Unauthorized
        );
        // Missing signature entirely.
        assert_eq!(
            evaluate(&hook(), Some("push"), None, PUSH_MAIN).unwrap(),
            Outcome::Unauthorized
        );
    }

    #[test]
    fn other_branch_is_ignored() {
        let body = br#"{"ref":"refs/heads/dev","after":"x"}"#;
        let sig = sign("s3cr3t", body);
        assert_eq!(
            evaluate(&hook(), Some("push"), Some(&sig), body).unwrap(),
            Outcome::Ignored("branch not matched")
        );
    }

    #[test]
    fn forward_sha_adds_arg() {
        let mut h = hook();
        h.action = "deploy-docs".into();
        h.forward_sha = true;
        let sig = sign("s3cr3t", PUSH_MAIN);
        let Outcome::Dispatch(d) = evaluate(&h, Some("push"), Some(&sig), PUSH_MAIN).unwrap()
        else {
            panic!("expected dispatch");
        };
        assert_eq!(d.args.get("sha").map(String::as_str), Some("0123abcdef"));
    }

    #[test]
    fn tampered_body_fails_signature() {
        let sig = sign("s3cr3t", PUSH_MAIN);
        let tampered = br#"{"ref":"refs/heads/main","after":"deadbeef"}"#;
        assert_eq!(
            evaluate(&hook(), Some("push"), Some(&sig), tampered).unwrap(),
            Outcome::Unauthorized
        );
    }

    #[test]
    fn release_published_dispatches() {
        let mut h = hook();
        h.trigger = Trigger::Release;
        let body = br#"{"action":"published","repository":{"full_name":"acme/site"}}"#;
        let sig = sign("s3cr3t", body);
        let out = evaluate(&h, Some("release"), Some(&sig), body).unwrap();
        assert_eq!(
            out,
            Outcome::Dispatch(Dispatch {
                action: "deploy-site".into(),
                target: "host1".into(),
                args: BTreeMap::new(),
                source: Some("github:acme/site".into()),
            })
        );
    }

    #[test]
    fn release_edited_dispatches() {
        // A rolling "latest" release re-uploads assets -> action=edited.
        let mut h = hook();
        h.trigger = Trigger::Release;
        let body = br#"{"action":"edited","repository":{"full_name":"acme/site"}}"#;
        let sig = sign("s3cr3t", body);
        assert!(matches!(
            evaluate(&h, Some("release"), Some(&sig), body).unwrap(),
            Outcome::Dispatch(_)
        ));
    }

    #[test]
    fn release_non_publish_action_ignored() {
        let mut h = hook();
        h.trigger = Trigger::Release;
        let body = br#"{"action":"deleted"}"#;
        let sig = sign("s3cr3t", body);
        assert_eq!(
            evaluate(&h, Some("release"), Some(&sig), body).unwrap(),
            Outcome::Ignored("release action ignored")
        );
        // A push event to a release-triggered hook is ignored too.
        assert_eq!(
            evaluate(
                &h,
                Some("push"),
                Some(&sign("s3cr3t", PUSH_MAIN)),
                PUSH_MAIN
            )
            .unwrap(),
            Outcome::Ignored("non-release event")
        );
    }

    #[test]
    fn any_trigger_dispatches_signed_request() {
        let mut h = hook();
        h.trigger = Trigger::Any;
        let body = br#"{"ci":"done"}"#;
        let sig = sign("s3cr3t", body);
        // No event header at all — a bare CI POST.
        let out = evaluate(&h, None, Some(&sig), body).unwrap();
        assert!(matches!(out, Outcome::Dispatch(_)));
        // Still gated by the signature.
        assert_eq!(
            evaluate(&h, None, Some("sha256=bad"), body).unwrap(),
            Outcome::Unauthorized
        );
    }
}
