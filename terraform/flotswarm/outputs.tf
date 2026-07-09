output "queue_urls" {
  description = "host -> SQS queue URL"
  value       = { for h, q in aws_sqs_queue.host : h => q.url }
}

output "queue_arns" {
  description = "host -> SQS queue ARN (attach per-host read policies to these)"
  value       = { for h, q in aws_sqs_queue.host : h => q.arn }
}

output "dlq_arn" {
  value = aws_sqs_queue.dlq.arn
}

output "routing_parameter" {
  value = aws_ssm_parameter.routing.name
}

output "hooks_endpoint" {
  description = "POST here as https://<this>/hooks/<id>"
  value       = aws_apigatewayv2_stage.default.invoke_url
}

output "api_id" {
  description = "HTTP API id (for attaching a custom domain / api mapping in the root)"
  value       = aws_apigatewayv2_api.hooks.id
}

output "alerts_topic_arn" {
  description = "flotswarm-alerts SNS topic ARN (null when alert_emails is empty)"
  value       = local.alerts_enabled ? aws_sns_topic.alerts[0].arn : null
}
