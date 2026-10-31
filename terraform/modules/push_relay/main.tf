# Push relay: the only place the APNs signing key is used. Desktops call it
# through an IAM-authenticated function URL to register their owner's devices
# and to notify them; it only sends to devices registered by the calling
# desktop, so the key never sits on a desktop that a user controls.

data "aws_caller_identity" "current" {}
data "aws_region" "current" {}
data "aws_partition" "current" {}

locals {
  function_name = "${var.name}-push-relay"
  ssm_arn       = "arn:${data.aws_partition.current.partition}:ssm:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:parameter"
}

data "archive_file" "code" {
  type        = "zip"
  source_dir  = var.source_dir
  output_path = "${path.root}/.build/push-relay.zip"
}

# Device tokens by desktop (an EC2 instance ID).
resource "aws_dynamodb_table" "devices" {
  name         = "${var.name}-push-devices"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "desktop"
  range_key    = "token"

  attribute {
    name = "desktop"
    type = "S"
  }

  attribute {
    name = "token"
    type = "S"
  }

  server_side_encryption {
    enabled = true
  }
}

resource "aws_cloudwatch_log_group" "relay" {
  name              = "/aws/lambda/${local.function_name}"
  retention_in_days = 14
}

data "aws_iam_policy_document" "lambda_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "relay" {
  name               = local.function_name
  assume_role_policy = data.aws_iam_policy_document.lambda_assume.json
}

data "aws_iam_policy_document" "relay" {
  statement {
    sid       = "Logs"
    actions   = ["logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["${aws_cloudwatch_log_group.relay.arn}:*"]
  }

  statement {
    sid       = "Devices"
    actions   = ["dynamodb:Query", "dynamodb:PutItem", "dynamodb:DeleteItem"]
    resources = [aws_dynamodb_table.devices.arn]
  }

  # The SecureString uses the AWS-managed aws/ssm key, which SSM may use for
  # callers in this account, so no separate KMS grant is needed.
  statement {
    sid       = "ApnsKey"
    actions   = ["ssm:GetParameter"]
    resources = ["${local.ssm_arn}${var.ssm_prefix}/apns/key_p8", "${local.ssm_arn}${var.ssm_prefix}/apns/key_id"]
  }
}

resource "aws_iam_role_policy" "relay" {
  name   = "push-relay"
  role   = aws_iam_role.relay.id
  policy = data.aws_iam_policy_document.relay.json
}

resource "aws_lambda_function" "relay" {
  function_name    = local.function_name
  description      = "Sends push notifications for desktops (holds the APNs key)."
  role             = aws_iam_role.relay.arn
  runtime          = "nodejs24.x"
  architectures    = ["arm64"]
  handler          = "index.handler"
  filename         = data.archive_file.code.output_path
  source_code_hash = data.archive_file.code.output_base64sha256
  memory_size      = 256
  timeout          = 20

  environment {
    variables = {
      ACCOUNT_ID          = data.aws_caller_identity.current.account_id
      DESKTOP_ROLE_PREFIX = var.desktop_role_prefix
      TABLE               = aws_dynamodb_table.devices.name
      KEY_PARAM           = "${var.ssm_prefix}/apns/key_p8"
      KEY_ID_PARAM        = "${var.ssm_prefix}/apns/key_id"
      TEAM_ID             = var.apns_team_id
      TOPIC               = var.apns_topic
      APNS_HOST           = var.apns_environment == "production" ? "api.push.apple.com" : "api.sandbox.push.apple.com"
    }
  }

  depends_on = [aws_cloudwatch_log_group.relay, aws_iam_role_policy.relay]
}

resource "aws_lambda_function_url" "relay" {
  function_name      = aws_lambda_function.relay.function_name
  authorization_type = "AWS_IAM"
}

# Desktops may call the relay only through its URL (signed with their instance
# role). Without the InvokedViaFunctionUrl condition a direct Invoke could
# forge the caller identity the relay checks.
data "aws_iam_policy_document" "desktop_invoke" {
  statement {
    sid       = "InvokeUrl"
    actions   = ["lambda:InvokeFunctionUrl"]
    resources = [aws_lambda_function.relay.arn]
    condition {
      test     = "StringEquals"
      variable = "lambda:FunctionUrlAuthType"
      values   = ["AWS_IAM"]
    }
  }

  statement {
    sid       = "InvokeViaUrlOnly"
    actions   = ["lambda:InvokeFunction"]
    resources = [aws_lambda_function.relay.arn]
    condition {
      test     = "Bool"
      variable = "lambda:InvokedViaFunctionUrl"
      values   = ["true"]
    }
  }
}

resource "aws_iam_role_policy" "desktop_invoke" {
  for_each = toset(var.desktop_role_names)

  name   = "push-relay-invoke"
  role   = each.value
  policy = data.aws_iam_policy_document.desktop_invoke.json
}

moved {
  from = aws_iam_role_policy.desktop_invoke
  to   = aws_iam_role_policy.desktop_invoke["cloud-desktops-dev-desktop"]
}

# Not a secret: where agentd sends notifications (read by Ansible).
resource "aws_ssm_parameter" "relay_url" {
  name  = "${var.ssm_prefix}/push/relay_url"
  type  = "String"
  value = aws_lambda_function_url.relay.function_url
}
