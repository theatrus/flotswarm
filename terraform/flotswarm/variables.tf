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
