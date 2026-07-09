# One SQS queue per host (the distributor broadcasts to all of them), plus a
# single shared dead-letter queue for the whole fleet. A message reaches the DLQ
# only after an action genuinely failed maxReceiveCount times — an allowlist miss
# is a clean delete by the agent, not a redrive.

resource "aws_sqs_queue" "dlq" {
  name                      = "flotswarm-dlq"
  message_retention_seconds = 1209600 # 14 days
}

resource "aws_sqs_queue" "host" {
  for_each = toset(var.hosts)

  name                       = "flotswarm-${each.key}"
  visibility_timeout_seconds = 900    # >= longest action runtime (deploys ~5m)
  message_retention_seconds  = 345600 # 4 days

  redrive_policy = jsonencode({
    deadLetterTargetArn = aws_sqs_queue.dlq.arn
    maxReceiveCount     = 3
  })
}

# Only the host queues may redrive into the shared DLQ.
resource "aws_sqs_queue_redrive_allow_policy" "dlq" {
  queue_url = aws_sqs_queue.dlq.id

  redrive_allow_policy = jsonencode({
    redrivePermission = "byQueue"
    sourceQueueArns   = [for q in aws_sqs_queue.host : q.arn]
  })
}
