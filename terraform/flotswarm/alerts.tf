# Optional alerting: a single SNS topic + CloudWatch alarms on the three surfaces
# that matter — the DLQ (an action failed), the distributor (a signed hook that
# then failed to fan out), and the HTTP API (5xx). Created only when alert_emails
# is set. Each email subscription must be confirmed via the mail AWS sends.

locals {
  alerts_enabled = length(var.alert_emails) > 0
  # Rich notifications (dispatch + outcome events → Discord/email) reuse the same
  # topic. Enabled when a Discord webhook or notify email is configured.
  notify_enabled = var.discord_webhook_ssm != "" || length(var.notify_email_to) > 0
  topic_enabled  = local.alerts_enabled || local.notify_enabled
  topic_arn      = local.topic_enabled ? aws_sns_topic.alerts[0].arn : null
}

resource "aws_sns_topic" "alerts" {
  count = local.topic_enabled ? 1 : 0
  name  = "flotswarm-alerts"
}

# Cross-account publish: agents in another account need the topic policy to
# allow their role to publish. Same-account publishers (the distributor and
# same-account agents) are covered by their own identity policies.
resource "aws_sns_topic_policy" "publishers" {
  count = local.topic_enabled && length(var.agent_publisher_arns) > 0 ? 1 : 0
  arn   = aws_sns_topic.alerts[0].arn
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "CrossAccountAgentPublish"
      Effect    = "Allow"
      Principal = { AWS = var.agent_publisher_arns }
      Action    = "sns:Publish"
      Resource  = aws_sns_topic.alerts[0].arn
    }]
  })
}

# Raw SNS email subscription (unformatted JSON, "AWS Notifications" sender).
# Suppressed when the notify Lambda is enabled — it emails the same recipients
# (alert_emails ∪ notify_email_to) as formatted, subject-lined SES mail. Kept
# only as a fallback when notify is off entirely.
resource "aws_sns_topic_subscription" "alerts_email" {
  for_each  = local.notify_enabled ? toset([]) : toset(var.alert_emails)
  topic_arn = aws_sns_topic.alerts[0].arn
  protocol  = "email"
  endpoint  = each.value
}

# An action failed maxReceiveCount times and landed in the DLQ.
resource "aws_cloudwatch_metric_alarm" "dlq_depth" {
  count             = local.alerts_enabled ? 1 : 0
  alarm_name        = "flotswarm-dlq-not-empty"
  alarm_description = "A dispatched flotswarm action failed enough times to reach the DLQ."

  namespace           = "AWS/SQS"
  metric_name         = "ApproximateNumberOfMessagesVisible"
  dimensions          = { QueueName = aws_sqs_queue.dlq.name }
  statistic           = "Maximum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 1
  comparison_operator = "GreaterThanOrEqualToThreshold"
  treat_missing_data  = "notBreaching"

  alarm_actions = [aws_sns_topic.alerts[0].arn]
  ok_actions    = [aws_sns_topic.alerts[0].arn]
}

# The distributor Lambda is erroring (a verified hook that can't fan out — invisible at the DLQ).
resource "aws_cloudwatch_metric_alarm" "distributor_errors" {
  count             = local.alerts_enabled ? 1 : 0
  alarm_name        = "flotswarm-distributor-errors"
  alarm_description = "The flotswarm distributor Lambda is returning errors."

  namespace           = "AWS/Lambda"
  metric_name         = "Errors"
  dimensions          = { FunctionName = aws_lambda_function.distributor.function_name }
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 1
  comparison_operator = "GreaterThanOrEqualToThreshold"
  treat_missing_data  = "notBreaching"

  alarm_actions = [aws_sns_topic.alerts[0].arn]
}

# The hooks HTTP API is returning 5xx.
resource "aws_cloudwatch_metric_alarm" "hooks_5xx" {
  count             = local.alerts_enabled ? 1 : 0
  alarm_name        = "flotswarm-hooks-5xx"
  alarm_description = "The flotswarm hooks HTTP API is returning 5xx responses."

  namespace           = "AWS/ApiGateway"
  metric_name         = "5xx"
  dimensions          = { ApiId = aws_apigatewayv2_api.hooks.id }
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 1
  comparison_operator = "GreaterThanOrEqualToThreshold"
  treat_missing_data  = "notBreaching"

  alarm_actions = [aws_sns_topic.alerts[0].arn]
}
