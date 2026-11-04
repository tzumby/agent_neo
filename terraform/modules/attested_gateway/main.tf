# The model gateway (services/gatewayd): an instance from an image nobody can
# log into (no SSH, SSM or user scripts; read-only root), so the provider key
# never reaches a desktop. Desktops pin its TLS certificate before sending it
# a prompt, from one of two places (var.attested):
# - its NitroTPM attestation, checked against the image's measurements. Only
#   non-burstable types can attest, hence a c7g.medium;
# - the fingerprint gatewayd publishes to an SSM parameter at boot, which
#   trusts whoever can write that parameter (the account's admins). Any type
#   will do, hence a t4g.micro.
# Its settings come from user data, which gatewayd reads itself (there's no
# cloud-init); none of them changes what it can see.

data "aws_caller_identity" "current" {}
data "aws_region" "current" {}
data "aws_partition" "current" {}

locals {
  ssm_arn       = "arn:${data.aws_partition.current.partition}:ssm:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:parameter"
  key_parameter = "${var.ssm_prefix}/llm/together_api_key"
  enabled       = var.ami != ""
  # NitroTPM attestation needs 8 KiB per NV index; burstable types (t3, t4g)
  # hold 2 KiB (aws/NitroTPM-Tools#7).
  instance_type = coalesce(var.instance_type, var.attested ? "c7g.medium" : "t4g.micro")
}

# Per desktop and day: calls, tokens and cost (gatewayd's daily budget).
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

resource "aws_security_group" "gateway" {
  name        = "${var.name}-model-gateway"
  description = "Attested model gateway: 8443 from desktops; out to HTTPS only"
  vpc_id      = var.vpc_id

  tags = {
    Name = "${var.name}-model-gateway"
  }
}

resource "aws_vpc_security_group_ingress_rule" "from_desktops" {
  security_group_id            = aws_security_group.gateway.id
  referenced_security_group_id = var.desktop_sg_id
  ip_protocol                  = "tcp"
  from_port                    = 8443
  to_port                      = 8443
}

# Together, STS, SSM and DynamoDB. (DNS, time and instance metadata are
# link-local and not subject to security groups.)
resource "aws_vpc_security_group_egress_rule" "https" {
  security_group_id = aws_security_group.gateway.id
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
  cidr_ipv4         = "0.0.0.0/0"
}

data "aws_iam_policy_document" "assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "gateway" {
  name                 = "${var.name}-model-gateway-instance"
  assume_role_policy   = data.aws_iam_policy_document.assume.json
  permissions_boundary = var.permissions_boundary
}

data "aws_iam_policy_document" "gateway" {
  # The SecureString uses the AWS-managed aws/ssm key (usable through SSM).
  statement {
    sid       = "ProviderKey"
    actions   = ["ssm:GetParameter"]
    resources = ["${local.ssm_arn}${local.key_parameter}"]
  }

  statement {
    sid       = "Usage"
    actions   = ["dynamodb:GetItem", "dynamodb:UpdateItem"]
    resources = [aws_dynamodb_table.usage.arn]
  }

  # This boot's certificate fingerprint, for desktops that pin it.
  statement {
    sid       = "PublishCertificate"
    actions   = ["ssm:PutParameter"]
    resources = ["${local.ssm_arn}${var.certificate_parameter}"]
  }
}

resource "aws_iam_role_policy" "gateway" {
  name   = "model-gateway"
  role   = aws_iam_role.gateway.id
  policy = data.aws_iam_policy_document.gateway.json
}

resource "aws_iam_instance_profile" "gateway" {
  name = "${var.name}-model-gateway"
  role = aws_iam_role.gateway.name
}

resource "aws_instance" "gateway" {
  count = local.enabled ? 1 : 0

  ami                         = var.ami
  instance_type               = local.instance_type
  subnet_id                   = var.subnet_id
  private_ip                  = var.private_ip
  associate_public_ip_address = false
  vpc_security_group_ids      = [aws_security_group.gateway.id]
  iam_instance_profile        = aws_iam_instance_profile.gateway.name

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_type = "gp3"
    encrypted   = true
  }

  # gatewayd's settings (services/gatewayd/src/config.rs).
  user_data = jsonencode({
    account_id            = data.aws_caller_identity.current.account_id
    desktop_role_prefix   = var.desktop_role_prefix
    audience              = var.audience
    usage_table           = aws_dynamodb_table.usage.name
    key_parameter         = local.key_parameter
    certificate_parameter = var.certificate_parameter
    daily_budget_usd      = var.daily_budget_usd
    prices                = var.prices
  })
  user_data_replace_on_change = true

  tags = {
    Name = "${var.name}-model-gateway"
    Role = "model-gateway"
  }

  lifecycle {
    precondition {
      condition     = !var.attested || var.measurements != ""
      error_message = "An attested gateway needs its image's measurements (published by scripts/build-gateway-image.sh)."
    }
  }
}

# What desktops need (the agentd role reads these): where the gateway is,
# how to know its certificate (the images to accept, or where it's
# published), and the audience to sign for.
resource "aws_ssm_parameter" "url" {
  count = local.enabled ? 1 : 0

  name  = "${var.ssm_prefix}/gateway/url"
  type  = "String"
  value = "https://${var.private_ip}:8443"
}

resource "aws_ssm_parameter" "measurements" {
  count = local.enabled && var.attested ? 1 : 0

  name  = "${var.ssm_prefix}/gateway/measurements"
  type  = "String"
  value = var.measurements
}

resource "aws_ssm_parameter" "certificate_parameter" {
  count = local.enabled && !var.attested ? 1 : 0

  name  = "${var.ssm_prefix}/gateway/certificate_parameter"
  type  = "String"
  value = var.certificate_parameter
}

# Where gatewayd publishes its certificate's fingerprint at each boot. Outside
# the project's prefix, so desktops may read it (their roles are denied
# everything under it); the gateway's role is the only one allowed to write it.
resource "aws_ssm_parameter" "certificate" {
  count = local.enabled ? 1 : 0

  name        = var.certificate_parameter
  description = "SHA-256 of the model gateway's TLS certificate (hex), written by gatewayd at boot"
  type        = "String"
  # Until the gateway's first boot. agentd pins nothing but a fingerprint.
  value = "pending"

  lifecycle {
    ignore_changes = [value]
  }
}

resource "aws_ssm_parameter" "audience" {
  name  = "${var.ssm_prefix}/gateway/audience"
  type  = "String"
  value = var.audience
}
