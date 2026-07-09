data "aws_caller_identity" "me" {}
data "aws_region" "current" {}

# --- Artifact bucket: the distributor Lambda zip is loaded from S3 by s3_key. ---

resource "aws_s3_bucket" "artifacts" {
  bucket = var.artifact_bucket
}

resource "aws_s3_bucket_public_access_block" "artifacts" {
  bucket                  = aws_s3_bucket.artifacts.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_object" "distributor" {
  bucket = aws_s3_bucket.artifacts.id
  key    = "distributor/bootstrap.zip"
  source = var.distributor_zip
  etag   = filemd5(var.distributor_zip)
}

# --- Distributor Lambda execution role: send to host queues, read /flotswarm/*. ---

data "aws_iam_policy_document" "distributor_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "distributor" {
  name               = "flotswarm-distributor"
  assume_role_policy = data.aws_iam_policy_document.distributor_assume.json
}

resource "aws_iam_role_policy_attachment" "distributor_logs" {
  role       = aws_iam_role.distributor.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy" "distributor" {
  name = "flotswarm-distributor"
  role = aws_iam_role.distributor.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "SendToHostQueues"
        Effect   = "Allow"
        Action   = "sqs:SendMessage"
        Resource = [for q in aws_sqs_queue.host : q.arn]
      },
      {
        Sid      = "ReadFlotswarmParams"
        Effect   = "Allow"
        Action   = "ssm:GetParameter"
        Resource = "arn:aws:ssm:${data.aws_region.current.name}:${data.aws_caller_identity.me.account_id}:parameter/flotswarm/*"
      },
      {
        # Decrypt hook SecureStrings — scoped to the SSM service via condition.
        Sid      = "DecryptViaSsm"
        Effect   = "Allow"
        Action   = "kms:Decrypt"
        Resource = "*"
        Condition = {
          StringEquals = { "kms:ViaService" = "ssm.${data.aws_region.current.name}.amazonaws.com" }
        }
      },
    ]
  })
}

# --- The Lambda (Rust on provided.al2023, aws-lc-rs crypto). ---

resource "aws_lambda_function" "distributor" {
  function_name = "flotswarm-distributor"
  role          = aws_iam_role.distributor.arn
  runtime       = "provided.al2023"
  handler       = "bootstrap"
  architectures = [var.lambda_architecture]

  s3_bucket        = aws_s3_bucket.artifacts.id
  s3_key           = aws_s3_object.distributor.key
  source_code_hash = filebase64sha256(var.distributor_zip)

  timeout     = 15
  memory_size = 128
}

# --- API Gateway HTTP API: POST /hooks/{id} -> distributor. ---

resource "aws_apigatewayv2_api" "hooks" {
  name          = "flotswarm-hooks"
  protocol_type = "HTTP"
}

resource "aws_apigatewayv2_integration" "distributor" {
  api_id                 = aws_apigatewayv2_api.hooks.id
  integration_type       = "AWS_PROXY"
  integration_uri        = aws_lambda_function.distributor.invoke_arn
  payload_format_version = "2.0"
}

resource "aws_apigatewayv2_route" "hook" {
  api_id    = aws_apigatewayv2_api.hooks.id
  route_key = "POST /hooks/{id}"
  target    = "integrations/${aws_apigatewayv2_integration.distributor.id}"
}

resource "aws_apigatewayv2_stage" "default" {
  api_id      = aws_apigatewayv2_api.hooks.id
  name        = "$default"
  auto_deploy = true
}

resource "aws_lambda_permission" "apigw" {
  statement_id  = "AllowAPIGatewayInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.distributor.function_name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${aws_apigatewayv2_api.hooks.execution_arn}/*/*"
}
