# Optional alerting: a single SNS topic + CloudWatch alarms on the three surfaces
# that matter — the DLQ (an action failed), the distributor (a signed hook that
# then failed to fan out), and the HTTP API (5xx). Created only when alert_emails
# is set. Each email subscription must be confirmed via the mail AWS sends.

locals {
  alerts_enabled = length(var.alert_emails) > 0
}

resource "aws_sns_topic" "alerts" {
  count = local.alerts_enabled ? 1 : 0
  name  = "flotswarm-alerts"
}

resource "aws_sns_topic_subscription" "alerts_email" {
  for_each  = toset(var.alert_emails)
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
