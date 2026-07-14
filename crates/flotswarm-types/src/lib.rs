//! flotswarm-types — the wire contract shared by the distributor (producer) and
//! the agent (consumer), plus the on-disk action-config schema the agent reads.
//!
//! This crate is deliberately **pure data + format validation only**. Anything
//! side-effecting — a `sha` arg's git repo-state check, actually running the
//! command — lives in the agent. That keeps this crate dependency-light and
//! unit-testable in isolation, and gives the distributor and agent one
//! type-checked definition of the message envelope.
//!
//! See the project README for the full design.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// A queue message: emitted by the distributor, consumed by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Dedup key (uuid). At-least-once delivery means the agent may see dupes.
    pub id: String,
    /// Action id; selects `/etc/flotswarm/actions.d/<action>.toml`.
    pub action: String,
    /// Host name, or `"all"` (default) for broadcast.
    #[serde(default = "default_target")]
    pub target: String,
    /// Action-specific args, validated against the action's schema.
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    /// Free-form provenance, e.g. `"github:acme/site"`.
    #[serde(default)]
    pub source: Option<String>,
    /// RFC3339 timestamp.
    pub ts: String,
}

fn default_target() -> String {
    "all".to_string()
}

impl Envelope {
    /// Whether this message is addressed to `host` (exact match or `"all"`).
    pub fn targets(&self, host: &str) -> bool {
        self.target == "all" || self.target == host
    }
}

/// A parsed `/etc/flotswarm/actions.d/<id>.toml`. The action id is the filename
/// stem; it is matched against [`Envelope::action`] by the agent.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionConfig {
    #[serde(default)]
    pub description: Option<String>,
    /// argv; `command[0]` must be an absolute path. Never a shell string.
    pub command: Vec<String>,
    /// Run the command as this user (via `sudo -u`); requires a sudoers rule.
    #[serde(default)]
    pub run_as: Option<String>,
    /// Max runtime, e.g. `"5m"`, `"30s"`. Parsed/enforced by the agent.
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Declared args. Anything in the message not listed here is rejected.
    #[serde(default)]
    pub args: BTreeMap<String, ArgSpec>,
    /// When the agent should emit an outcome notification for this action.
    #[serde(default)]
    pub notify: NotifyMode,
}

/// Per-action notification preference (the `notify` field in an action TOML).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyMode {
    /// Emit on both success and failure.
    #[default]
    Always,
    /// Emit only when the action fails.
    OnError,
    /// Never emit an outcome notification.
    Never,
}

impl NotifyMode {
    /// Whether to publish given the action's success/failure.
    pub fn should_emit(&self, ok: bool) -> bool {
        match self {
            NotifyMode::Always => true,
            NotifyMode::OnError => !ok,
            NotifyMode::Never => false,
        }
    }
}

/// Notification event published to the SNS bus by the distributor (dispatch) and
/// the agents (outcome). Consumed by the notify Lambda. `kind`-tagged so one
/// topic carries both and the relay can format each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NotifyEvent {
    /// A hook fired at the distributor.
    Dispatch {
        hook: String,
        action: String,
        /// Host queues the envelope was sent to.
        targets: Vec<String>,
        #[serde(default)]
        source: Option<String>,
        /// `"dispatched"`, `"ignored:<reason>"`, `"rejected:<reason>"`.
        outcome: String,
    },
    /// An agent finished running an action.
    Outcome {
        host: String,
        action: String,
        #[serde(default)]
        source: Option<String>,
        /// Envelope id (dedup key).
        id: String,
        ok: bool,
        #[serde(default)]
        exit_code: Option<i32>,
        duration_s: f64,
        /// Last lines of combined stdout+stderr (bounded).
        #[serde(default)]
        tail: String,
    },
}

/// The schema for one accepted arg. The TOML `type = "..."` field selects the
/// variant. `argv` tokens are appended to the command when the arg is present,
/// with `${value}` replaced by the validated value.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ArgSpec {
    /// A free string constrained by a mandatory regex.
    String {
        #[serde(default)]
        required: bool,
        pattern: String,
        #[serde(default)]
        argv: Vec<String>,
    },
    /// One of a fixed set of values.
    Enum {
        #[serde(default)]
        required: bool,
        values: Vec<String>,
        #[serde(default)]
        argv: Vec<String>,
    },
    /// A git commit id: hex format checked here; repo-state checked by the agent.
    Sha {
        #[serde(default)]
        required: bool,
        repo: String,
        ancestor_of: String,
        #[serde(default)]
        argv: Vec<String>,
    },
}

impl ArgSpec {
    pub fn required(&self) -> bool {
        match self {
            ArgSpec::String { required, .. }
            | ArgSpec::Enum { required, .. }
            | ArgSpec::Sha { required, .. } => *required,
        }
    }

    fn argv(&self) -> &[String] {
        match self {
            ArgSpec::String { argv, .. }
            | ArgSpec::Enum { argv, .. }
            | ArgSpec::Sha { argv, .. } => argv,
        }
    }
}

/// A git repo-state check the **agent** must run before executing (this crate
/// can't shell out). Produced by [`ActionConfig::plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaCheck {
    pub arg: String,
    pub value: String,
    /// Repo to check in.
    pub repo: String,
    /// The value must be an ancestor of this ref (e.g. `origin/main`).
    pub ancestor_of: String,
}

/// The result of validating a message against an action: the argv to run, plus
/// any semantic checks the agent must still perform before executing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecPlan {
    pub argv: Vec<String>,
    pub sha_checks: Vec<ShaCheck>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("command is empty")]
    EmptyCommand,
    #[error("command[0] must be an absolute path, got {0:?}")]
    NonAbsoluteCommand(String),
    #[error("unknown arg {0:?} (not declared by this action)")]
    UnknownArg(String),
    #[error("missing required arg {0:?}")]
    MissingArg(String),
    #[error("arg {arg:?}: value {value:?} does not match pattern {pattern:?}")]
    PatternMismatch {
        arg: String,
        value: String,
        pattern: String,
    },
    #[error("arg {arg:?}: value {value:?} is not one of {values:?}")]
    NotInEnum {
        arg: String,
        value: String,
        values: Vec<String>,
    },
    #[error("arg {arg:?}: {value:?} is not a valid git sha")]
    BadSha { arg: String, value: String },
    #[error("arg {arg:?}: invalid regex {pattern:?}: {message}")]
    BadPattern {
        arg: String,
        pattern: String,
        message: String,
    },
}

impl ActionConfig {
    /// Parse an action file's TOML.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Strictly validate `env.args` against this action's schema and assemble the
    /// argv to run. Does **not** run git or sudo — the caller must still satisfy
    /// every [`ExecPlan::sha_checks`] entry before executing.
    pub fn plan(&self, env: &Envelope) -> Result<ExecPlan, PlanError> {
        if self.command.is_empty() {
            return Err(PlanError::EmptyCommand);
        }
        let cmd0 = &self.command[0];
        if !cmd0.starts_with('/') {
            return Err(PlanError::NonAbsoluteCommand(cmd0.clone()));
        }

        // Strict: reject any message arg not declared by this action.
        for key in env.args.keys() {
            if !self.args.contains_key(key) {
                return Err(PlanError::UnknownArg(key.clone()));
            }
        }

        let mut argv = self.command.clone();
        let mut sha_checks = Vec::new();

        // Validate declared args in a stable (BTreeMap) order.
        for (name, spec) in &self.args {
            let value = match env.args.get(name) {
                None => {
                    if spec.required() {
                        return Err(PlanError::MissingArg(name.clone()));
                    }
                    continue;
                }
                Some(v) => v,
            };

            match spec {
                ArgSpec::String { pattern, .. } => {
                    let re =
                        regex::Regex::new(pattern).map_err(|source| PlanError::BadPattern {
                            arg: name.clone(),
                            pattern: pattern.clone(),
                            message: source.to_string(),
                        })?;
                    if !re.is_match(value) {
                        return Err(PlanError::PatternMismatch {
                            arg: name.clone(),
                            value: value.clone(),
                            pattern: pattern.clone(),
                        });
                    }
                }
                ArgSpec::Enum { values, .. } => {
                    if !values.contains(value) {
                        return Err(PlanError::NotInEnum {
                            arg: name.clone(),
                            value: value.clone(),
                            values: values.clone(),
                        });
                    }
                }
                ArgSpec::Sha {
                    repo, ancestor_of, ..
                } => {
                    if !is_git_sha(value) {
                        return Err(PlanError::BadSha {
                            arg: name.clone(),
                            value: value.clone(),
                        });
                    }
                    sha_checks.push(ShaCheck {
                        arg: name.clone(),
                        value: value.clone(),
                        repo: repo.clone(),
                        ancestor_of: ancestor_of.clone(),
                    });
                }
            }

            // Append this arg's argv tokens, substituting the validated value.
            // Values only ever become whole argv tokens — never shell-parsed.
            for tok in spec.argv() {
                argv.push(tok.replace("${value}", value));
            }
        }

        Ok(ExecPlan { argv, sha_checks })
    }
}

/// Format check for a git commit id: 7–40 lowercase hex chars (`^[0-9a-f]{7,40}$`).
pub fn is_git_sha(s: &str) -> bool {
    (7..=40).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(action: &str, args: &[(&str, &str)]) -> Envelope {
        Envelope {
            id: "demo".into(),
            action: action.into(),
            target: "host1".into(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            source: None,
            ts: "1970-01-01T00:00:00Z".into(),
        }
    }

    const NO_ARGS: &str = r#"
        command = ["/usr/local/bin/deploy-site"]
        run_as  = "deploy"
        timeout = "5m"
    "#;

    const SHA_ARG: &str = r#"
        command = ["/usr/local/bin/deploy-docs"]
        run_as  = "deploy"

        [args.sha]
        type        = "sha"
        required    = false
        argv        = ["--sha", "${value}"]
        repo        = "/srv/www/docs"
        ancestor_of = "origin/main"
    "#;

    #[test]
    fn targets_all_and_exact() {
        let e = env_with("x", &[]);
        assert!(e.targets("host1"));
        assert!(!e.targets("host2"));
        let mut e2 = e.clone();
        e2.target = "all".into();
        assert!(e2.targets("host2"));
    }

    #[test]
    fn no_args_action_plans_to_bare_command() {
        let a = ActionConfig::from_toml_str(NO_ARGS).unwrap();
        let plan = a.plan(&env_with("deploy-site", &[])).unwrap();
        assert_eq!(plan.argv, ["/usr/local/bin/deploy-site"]);
        assert!(plan.sha_checks.is_empty());
    }

    #[test]
    fn no_args_action_rejects_stray_args() {
        let a = ActionConfig::from_toml_str(NO_ARGS).unwrap();
        let err = a.plan(&env_with("x", &[("sha", "abc1234")])).unwrap_err();
        assert_eq!(err, PlanError::UnknownArg("sha".into()));
    }

    #[test]
    fn sha_arg_valid_appends_argv_and_defers_check() {
        let a = ActionConfig::from_toml_str(SHA_ARG).unwrap();
        let plan = a.plan(&env_with("x", &[("sha", "0123abcdef")])).unwrap();
        assert_eq!(
            plan.argv,
            ["/usr/local/bin/deploy-docs", "--sha", "0123abcdef"]
        );
        assert_eq!(plan.sha_checks.len(), 1);
        assert_eq!(plan.sha_checks[0].ancestor_of, "origin/main");
    }

    #[test]
    fn sha_arg_absent_is_allowed_when_optional() {
        let a = ActionConfig::from_toml_str(SHA_ARG).unwrap();
        let plan = a.plan(&env_with("x", &[])).unwrap();
        assert_eq!(plan.argv, ["/usr/local/bin/deploy-docs"]);
    }

    #[test]
    fn bad_sha_is_rejected() {
        let a = ActionConfig::from_toml_str(SHA_ARG).unwrap();
        // Uppercase + too short + non-hex all fail.
        for bad in ["DEADBEEF", "xyz", "g0123ab", "0123AB"] {
            let err = a.plan(&env_with("x", &[("sha", bad)])).unwrap_err();
            assert!(
                matches!(err, PlanError::BadSha { .. }),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn non_absolute_command_rejected() {
        let a = ActionConfig::from_toml_str(r#"command = ["deploy.sh"]"#).unwrap();
        assert_eq!(
            a.plan(&env_with("x", &[])).unwrap_err(),
            PlanError::NonAbsoluteCommand("deploy.sh".into())
        );
    }

    #[test]
    fn envelope_json_roundtrips_with_defaults() {
        let json = r#"{"id":"1","action":"deploy-x","ts":"1970-01-01T00:00:00Z"}"#;
        let e: Envelope = serde_json::from_str(json).unwrap();
        assert_eq!(e.target, "all"); // defaulted
        assert!(e.args.is_empty());
        let back = serde_json::to_string(&e).unwrap();
        let e2: Envelope = serde_json::from_str(&back).unwrap();
        assert_eq!(e, e2);
    }

    #[test]
    fn is_git_sha_bounds() {
        assert!(is_git_sha("abc1234")); // 7
        assert!(is_git_sha(&"a".repeat(40))); // 40
        assert!(!is_git_sha("abc123")); // 6
        assert!(!is_git_sha(&"a".repeat(41))); // 41
        assert!(!is_git_sha("ABC1234")); // uppercase
    }
}
