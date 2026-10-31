# Desktop instance role. The control-plane and Packer roles are added in later
# milestones.

data "aws_partition" "current" {}

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
  name               = "${var.name}-desktop"
  assume_role_policy = data.aws_iam_policy_document.ec2_assume.json
}

resource "aws_iam_role_policy_attachment" "desktop" {
  for_each = toset([
    "AmazonSSMManagedInstanceCore",
    "CloudWatchAgentServerPolicy",
  ])

  role       = aws_iam_role.desktop.name
  policy_arn = "arn:${data.aws_partition.current.partition}:iam::aws:policy/${each.key}"
}

resource "aws_iam_instance_profile" "desktop" {
  name = "${var.name}-desktop"
  role = aws_iam_role.desktop.name
}

# AmazonSSMManagedInstanceCore allows ssm:GetParameter on every parameter, and
# AWS's default SSM key decrypts for anyone in the account. Anything on a
# desktop that reaches the instance metadata service (an agent's shell, say)
# could read our secrets: model keys, the APNs key. Desktops never need them
# (Ansible reads them on the controller), so deny them outright.
data "aws_caller_identity" "current" {}

data "aws_iam_policy_document" "desktop_deny_secrets" {
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

resource "aws_iam_role_policy" "desktop_deny_secrets" {
  name   = "deny-project-secrets"
  role   = aws_iam_role.desktop.id
  policy = data.aws_iam_policy_document.desktop_deny_secrets.json
}
