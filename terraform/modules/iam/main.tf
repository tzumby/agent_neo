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

data "aws_iam_policy_document" "desktop_ssm_transfer_read" {
  statement {
    actions   = ["s3:GetObject"]
    resources = ["${var.ssm_transfer_bucket_arn}/*"]
  }
}

resource "aws_iam_role_policy" "desktop_ssm_transfer_read" {
  name   = "ssm-transfer-read"
  role   = aws_iam_role.desktop.id
  policy = data.aws_iam_policy_document.desktop_ssm_transfer_read.json
}

resource "aws_iam_instance_profile" "desktop" {
  name = "${var.name}-desktop"
  role = aws_iam_role.desktop.name
}
