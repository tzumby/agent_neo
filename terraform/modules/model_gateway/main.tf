# Model gateway: the only place the model provider's API key is used.
# Desktops call it through an IAM-authenticated function URL (SigV4 with their
# instance role) instead of holding the key. It allows known models, keeps a
# daily budget per desktop and logs usage, never prompt content.

data "aws_caller_identity" "current" {}
data "aws_region" "current" {}
data "aws_partition" "current" {}

locals {
  function_name = "${var.name}-model-gateway"
  ssm_arn       = "arn:${data.aws_partition.current.partition}:ssm:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:parameter"
}

data "archive_file" "code" {
  type        = "zip"
  source_dir  = var.source_dir
  output_path = "${path.root}/.build/model-gateway.zip"
}

# Spend per desktop (an EC2 instance ID) per day; rows expire after 90 days.
resource "aws_dynamodb_table" "usage" {
  name         = "${var.name}-model-usage"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "desktop"
  range_key    = "day"

  attribute {
    name = "desktop"
    type = "S"
  }

  attribute {
    name = "day"
    type = "S"
  }

  ttl {
    attribute_name = "expires"
    enabled        = true
  }

  server_side_encryption {
    enabled = true
  }
}

resource "aws_cloudwatch_log_group" "gateway" {
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

resource "aws_iam_role" "gateway" {
  name               = local.function_name
  assume_role_policy = data.aws_iam_policy_document.lambda_assume.json
}

data "aws_iam_policy_document" "gateway" {
  statement {
    sid       = "Logs"
    actions   = ["logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["${aws_cloudwatch_log_group.gateway.arn}:*"]
  }

  statement {
    sid       = "Usage"
    actions   = ["dynamodb:GetItem", "dynamodb:UpdateItem"]
    resources = [aws_dynamodb_table.usage.arn]
  }

  # The SecureString uses the AWS-managed aws/ssm key (usable through SSM).
  statement {
    sid       = "ProviderKey"
    actions   = ["ssm:GetParameter"]
    resources = ["${local.ssm_arn}${var.ssm_prefix}/llm/together_api_key"]
  }
}

resource "aws_iam_role_policy" "gateway" {
  name   = "model-gateway"
  role   = aws_iam_role.gateway.id
  policy = data.aws_iam_policy_document.gateway.json
}

resource "aws_lambda_function" "gateway" {
  function_name    = local.function_name
  description      = "Model calls for desktops (holds the provider key, budgets per desktop)."
  role             = aws_iam_role.gateway.arn
  runtime          = "nodejs24.x"
  architectures    = ["arm64"]
  handler          = "index.handler"
  filename         = data.archive_file.code.output_path
  source_code_hash = data.archive_file.code.output_base64sha256
  memory_size      = 256
  # A long model answer can take minutes; agentd waits up to 200 s.
  timeout = 210

  environment {
    variables = {
      ACCOUNT_ID          = data.aws_caller_identity.current.account_id
      DESKTOP_ROLE_PREFIX = var.desktop_role_prefix
      TABLE               = aws_dynamodb_table.usage.name
      KEY_PARAM           = "${var.ssm_prefix}/llm/together_api_key"
      PRICES              = jsonencode(var.prices)
      DAILY_BUDGET_USD    = tostring(var.daily_budget_usd)
    }
  }

  depends_on = [aws_cloudwatch_log_group.gateway, aws_iam_role_policy.gateway]
}

resource "aws_lambda_function_url" "gateway" {
  function_name      = aws_lambda_function.gateway.function_name
  authorization_type = "AWS_IAM"
}

# Desktops may call the gateway only through its URL (signed with their
# instance role). Without the InvokedViaFunctionUrl condition a direct Invoke
# could forge the caller identity the gateway checks.
data "aws_iam_policy_document" "desktop_invoke" {
  statement {
    sid       = "InvokeUrl"
    actions   = ["lambda:InvokeFunctionUrl"]
    resources = [aws_lambda_function.gateway.arn]
    condition {
      test     = "StringEquals"
      variable = "lambda:FunctionUrlAuthType"
      values   = ["AWS_IAM"]
    }
  }

  statement {
    sid       = "InvokeViaUrlOnly"
    actions   = ["lambda:InvokeFunction"]
    resources = [aws_lambda_function.gateway.arn]
    condition {
      test     = "Bool"
      variable = "lambda:InvokedViaFunctionUrl"
      values   = ["true"]
    }
  }
}

resource "aws_iam_role_policy" "desktop_invoke" {
  for_each = toset(var.desktop_role_names)

  name   = "model-gateway-invoke"
  role   = each.value
  policy = data.aws_iam_policy_document.desktop_invoke.json
}

# Not a secret: where agentd sends model calls (read by Ansible).
resource "aws_ssm_parameter" "url" {
  name  = "${var.ssm_prefix}/model-gateway/url"
  type  = "String"
  value = aws_lambda_function_url.gateway.function_url
}
