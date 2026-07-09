# The routing table the distributor reads to know which host queues exist.
# Rendered straight from the queue resources, so it can never drift from them —
# one `apply` writes both the queues and this table. Adding a host (to
# var.hosts) updates this parameter; the Lambda is never redeployed.

resource "aws_ssm_parameter" "routing" {
  name        = "/flotswarm/routing"
  description = "flotswarm host -> SQS queue URL map (read by flotswarm-distributor)"
  type        = "String"

  value = jsonencode({
    version = 1
    queues  = { for h, q in aws_sqs_queue.host : h => q.url }
  })
}
