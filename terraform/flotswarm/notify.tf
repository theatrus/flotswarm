# flotswarm-notify: SNS-subscribed Python Lambda that relays NotifyEvents
# (dispatch/outcome, published by the distributor and agents) plus CloudWatch
# alarm messages to Discord (all) and email via SES (failures/alarms). Created
# only when notify is enabled. Config is env/SSM — no infra specifics here.

data "archive_file" "notify" {
  count       = local.notify_enabled ? 1 : 0
  type        = "zip"
  source_file = "${path.module}/notify/handler.py"
  output_path = "${path.module}/notify/.build/notify.zip"
}

data "aws_iam_policy_document" "notify_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "notify" {
  count              = local.notify_enabled ? 1 : 0
  name               = "flotswarm-notify"
  assume_role_policy = data.aws_iam_policy_document.notify_assume.json
}

resource "aws_iam_role_policy_attachment" "notify_logs" {
  count      = local.notify_enabled ? 1 : 0
  role       = aws_iam_role.notify[0].name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy" "notify" {
  count = local.notify_enabled ? 1 : 0
  name  = "flotswarm-notify"
  role  = aws_iam_role.notify[0].id
  # One statement per conditional so each list is type-homogeneous (Terraform
  # can't unify differently-shaped objects across a ?:).
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = concat(
      var.discord_webhook_ssm != "" ? [{
        Sid      = "ReadDiscordWebhook"
        Effect   = "Allow"
        Action   = "ssm:GetParameter"
        Resource = "arn:aws:ssm:${data.aws_region.current.name}:${data.aws_caller_identity.me.account_id}:parameter${var.discord_webhook_ssm}"
      }] : [],
      var.discord_webhook_ssm != "" ? [{
        Sid       = "DecryptViaSsm"
        Effect    = "Allow"
        Action    = "kms:Decrypt"
        Resource  = "*"
        Condition = { StringEquals = { "kms:ViaService" = "ssm.${data.aws_region.current.name}.amazonaws.com" } }
      }] : [],
      var.notify_email_from != "" ? [{
        Sid      = "SendNotifyEmail"
        Effect   = "Allow"
        Action   = ["ses:SendEmail"]
        Resource = "*"
      }] : [],
    )
  })
}

resource "aws_lambda_function" "notify" {
  count         = local.notify_enabled ? 1 : 0
  function_name = "flotswarm-notify"
  role          = aws_iam_role.notify[0].arn
  runtime       = "python3.12"
  handler       = "handler.handler"
  architectures = ["arm64"]

  filename         = data.archive_file.notify[0].output_path
  source_code_hash = data.archive_file.notify[0].output_base64sha256

  timeout     = 15
  memory_size = 128

  environment {
    variables = {
      DISCORD_WEBHOOK_SSM = var.discord_webhook_ssm
      EMAIL_FROM          = var.notify_email_from
      # Formatted email goes to notify_email_to plus alert_emails — the latter
      # would otherwise only get the raw SNS email subscription (suppressed in
      # alerts.tf when notify is enabled).
      EMAIL_TO = join(",", distinct(concat(var.notify_email_to, var.alert_emails)))
    }
  }
}

resource "aws_sns_topic_subscription" "notify" {
  count     = local.notify_enabled ? 1 : 0
  topic_arn = aws_sns_topic.alerts[0].arn
  protocol  = "lambda"
  endpoint  = aws_lambda_function.notify[0].arn
}

resource "aws_lambda_permission" "notify_sns" {
  count         = local.notify_enabled ? 1 : 0
  statement_id  = "AllowSNSInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.notify[0].function_name
  principal     = "sns.amazonaws.com"
  source_arn    = aws_sns_topic.alerts[0].arn
}
