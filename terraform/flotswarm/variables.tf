variable "hosts" {
  description = "Hosts that run a flotswarm-agent; one SQS queue is created per host and the routing table lists them all."
  type        = list(string)
}

variable "distributor_zip" {
  description = "Path to the built distributor Lambda zip (from `make lambda` / scripts/build-lambda.sh)."
  type        = string
}

variable "artifact_bucket" {
  description = "Name of the (created) S3 bucket the distributor Lambda zip is uploaded to. Must be globally unique."
  type        = string
}

variable "lambda_architecture" {
  description = "Lambda CPU architecture; must match how the zip was built."
  type        = string
  default     = "arm64"
}

variable "alert_emails" {
  description = "Emails subscribed to flotswarm-alerts (DLQ depth, distributor errors, API 5xx). Empty = no alerting resources. Each subscription must be confirmed via the mail AWS sends."
  type        = list(string)
  default     = []
}

variable "discord_webhook_ssm" {
  description = "SSM parameter name (SecureString) holding the Discord webhook URL. Empty = no Discord relay. The value is set out of band / in tfinfra, never here."
  type        = string
  default     = ""
}

variable "notify_email_from" {
  description = "Verified SES sender for notify email (empty = no email). E.g. flotswarm@example.com."
  type        = string
  default     = ""
}

variable "notify_email_to" {
  description = "Recipients for notify email (failed outcomes, firing alarms, rejected hooks)."
  type        = list(string)
  default     = []
}

variable "agent_publisher_arns" {
  description = "Cross-account IAM role ARNs (member-account agents) allowed to publish outcome events to the notify topic. Same-account agents are granted via their own identity policy in the consuming root."
  type        = list(string)
  default     = []
}
