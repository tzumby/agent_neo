# One desktop's own AWS identity: an instance role and profile. Its own role
# (not one shared by every desktop) lets each policy name exactly this desktop.
# It gets SSM (for Ansible and Session Manager) and, from the relay and gateway
# modules, permission to call them. No access to the transfer bucket: Ansible
# hands it presigned URLs. Never our secrets.

data "aws_partition" "current" {}
data "aws_caller_identity" "current" {}

data "aws_iam_policy_document" "ec2_assume" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "desktop" {
  name                 = var.role_name
  assume_role_policy   = data.aws_iam_policy_document.ec2_assume.json
  permissions_boundary = var.permissions_boundary

  tags = {
    User = var.user
  }
}

resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.desktop.name
  policy_arn = "arn:${data.aws_partition.current.partition}:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

# AmazonSSMManagedInstanceCore allows ssm:GetParameter on every parameter, and
# AWS's default SSM key decrypts for anyone in the account: deny ours.
data "aws_iam_policy_document" "deny_secrets" {
  statement {
    sid    = "NoProjectParameters"
    effect = "Deny"
    actions = [
      "ssm:GetParameter",
      "ssm:GetParameters",
      "ssm:GetParametersByPath",
      "ssm:GetParameterHistory",
    ]
    resources = ["arn:${data.aws_partition.current.partition}:ssm:*:${data.aws_caller_identity.current.account_id}:parameter${var.ssm_parameter_prefix}/*"]
  }
}

resource "aws_iam_role_policy" "deny_secrets" {
  name   = "deny-project-secrets"
  role   = aws_iam_role.desktop.id
  policy = data.aws_iam_policy_document.deny_secrets.json
}

# Its own settings (a customer deployment's front door): read by root on the
# desktop at each start of front-door and cloudflared, so the customer can
# set or rotate them without us. SecureStrings use the aws/ssm key.
data "aws_iam_policy_document" "own_parameters" {
  count = var.own_parameters_path != "" ? 1 : 0

  statement {
    sid       = "OwnParameters"
    actions   = ["ssm:GetParameter", "ssm:GetParameters"]
    resources = ["arn:${data.aws_partition.current.partition}:ssm:*:${data.aws_caller_identity.current.account_id}:parameter${var.own_parameters_path}/*"]
  }
}

resource "aws_iam_role_policy" "own_parameters" {
  count = var.own_parameters_path != "" ? 1 : 0

  name   = "own-parameters"
  role   = aws_iam_role.desktop.id
  policy = data.aws_iam_policy_document.own_parameters[0].json
}

resource "aws_iam_instance_profile" "desktop" {
  name = var.role_name
  role = aws_iam_role.desktop.name
}
