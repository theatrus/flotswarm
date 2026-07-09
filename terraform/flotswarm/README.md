# `flotswarm` Terraform module

Stands up the AWS side of flotswarm: per-host SQS queues + a shared DLQ, the
routing table (SSM, rendered from the queue set), the distributor Lambda + its
role, and an HTTP API (`POST /hooks/{id}`). Crypto in the Lambda is aws-lc-rs.

Your infrastructure repo consumes this from a thin root that owns the provider
and any host-specific IAM wiring.

## Usage

```hcl
module "flotswarm" {
  source = "github.com/theatrus/flotswarm//terraform/flotswarm" # or a local path

  hosts           = ["web1"]                     # one queue per host
  artifact_bucket = "my-flotswarm-artifacts"     # globally-unique S3 bucket
  distributor_zip = abspath("../path/to/bootstrap.zip") # from `make lambda`
}

# Grant each host read on its own queue (EC2 example — attach to an instance role):
resource "aws_iam_role_policy" "agent" {
  role = "role-web1"
  policy = jsonencode({ Version = "2012-10-17", Statement = [{
    Effect   = "Allow"
    Action   = ["sqs:ReceiveMessage", "sqs:DeleteMessage", "sqs:GetQueueAttributes"]
    Resource = module.flotswarm.queue_arns["web1"]
  }] })
}
```

## Inputs

| Name | Description |
|------|-------------|
| `hosts` | Hosts running an agent; one SQS queue is created per host. |
| `artifact_bucket` | Name of the (created) S3 bucket for the Lambda zip. Globally unique. |
| `distributor_zip` | Path to the built distributor zip (`make lambda`). Pass an absolute path. |
| `lambda_architecture` | `arm64` (default) or `x86_64` — must match the built zip. |

## Outputs

`queue_urls`, `queue_arns` (host→…), `dlq_arn`, `routing_parameter`, `hooks_endpoint`.

## Out of scope (wire these in the root)

Per-host read IAM (above), hook secrets (SSM SecureString `/flotswarm/hooks/<id>`,
created out-of-band — never in Terraform state), a custom domain, and alerting.
