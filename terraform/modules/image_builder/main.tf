# A temporary Amazon Linux 2023 instance that builds attestable images
# (scripts/build-gateway-image.sh launches and terminates it). SSM runs the
# build; the role can turn the raw image into an AMI and publish the image's
# measurements, and nothing else of ours.

data "aws_caller_identity" "current" {}
data "aws_region" "current" {}
data "aws_partition" "current" {}

locals {
  ssm_arn = "arn:${data.aws_partition.current.partition}:ssm:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:parameter"
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

resource "aws_iam_role" "builder" {
  name                 = "${var.name}-image-builder"
  assume_role_policy   = data.aws_iam_policy_document.assume.json
  permissions_boundary = var.permissions_boundary
}

resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.builder.name
  policy_arn = "arn:${data.aws_partition.current.partition}:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

data "aws_iam_policy_document" "builder" {
  # coldsnap: the raw image to an EBS snapshot (EBS direct APIs).
  statement {
    sid       = "Snapshot"
    actions   = ["ebs:StartSnapshot", "ebs:PutSnapshotBlock", "ebs:CompleteSnapshot"]
    resources = ["arn:${data.aws_partition.current.partition}:ec2:*::snapshot/*"]
  }

  statement {
    sid       = "Image"
    actions   = ["ec2:RegisterImage", "ec2:DescribeImages", "ec2:DescribeSnapshots", "ec2:CreateTags"]
    resources = ["*"]
  }

  statement {
    sid       = "PublishMeasurements"
    actions   = ["ssm:PutParameter", "ssm:AddTagsToResource"]
    resources = ["${local.ssm_arn}${var.ssm_prefix}/gateway/images/*"]
  }

  # AmazonSSMManagedInstanceCore allows reading every parameter; not ours.
  statement {
    sid       = "NoSecrets"
    effect    = "Deny"
    actions   = ["ssm:GetParameter", "ssm:GetParameters", "ssm:GetParametersByPath"]
    resources = ["${local.ssm_arn}${var.ssm_prefix}/*"]
  }
}

resource "aws_iam_role_policy" "builder" {
  name   = "image-builder"
  role   = aws_iam_role.builder.id
  policy = data.aws_iam_policy_document.builder.json
}

resource "aws_iam_instance_profile" "builder" {
  name = "${var.name}-image-builder"
  role = aws_iam_role.builder.name
}

# Out only (packages, crates, AWS APIs); nothing comes in.
resource "aws_security_group" "builder" {
  name        = "${var.name}-image-builder"
  description = "Image builder: outbound only"
  vpc_id      = var.vpc_id

  tags = {
    Name = "${var.name}-image-builder"
  }
}

resource "aws_vpc_security_group_egress_rule" "builder_all" {
  security_group_id = aws_security_group.builder.id
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}
