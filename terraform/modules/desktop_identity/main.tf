# The desktop's own AWS identity: an instance role and profile. It gets SSM
# (for Ansible and Session Manager) and nothing else. No access to the
# transfer bucket: Ansible hands it presigned URLs. Never the project's
# secrets.

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
  name               = var.role_name
  assume_role_policy = data.aws_iam_policy_document.ec2_assume.json
}

resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.desktop.name
  policy_arn = "arn:${data.aws_partition.current.partition}:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

# AmazonSSMManagedInstanceCore allows ssm:GetParameter on every parameter, and
# AWS's default SSM key decrypts for anyone in the account. Anything on the
# desktop that reaches the instance metadata service (the agent's shell, say)
# could read the model key and the Tailscale auth key. The desktop never needs
# them (Ansible reads them on your machine), so deny them outright.
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

resource "aws_iam_instance_profile" "desktop" {
  name = var.role_name
  role = aws_iam_role.desktop.name
}
