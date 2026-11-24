locals {
  desktop_backup_tags = { NeoAgentBackup = local.name }
  backup_snapshot_arn = "arn:${data.aws_partition.backup.partition}:ec2:${var.region}::snapshot/*"
}

data "aws_partition" "backup" {}

data "aws_caller_identity" "backup" {
  count = var.enable_desktop_backups ? 1 : 0
}

resource "aws_iam_role" "desktop_backup" {
  count = var.enable_desktop_backups ? 1 : 0
  name  = "${local.name}-snapshots"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Action    = "sts:AssumeRole"
      Principal = { Service = "dlm.amazonaws.com" }
    }]
  })
}

resource "aws_iam_role_policy" "desktop_backup" {
  count = var.enable_desktop_backups ? 1 : 0
  name  = "desktop-snapshots"
  role  = aws_iam_role.desktop_backup[0].id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ec2:DescribeAvailabilityZones", "ec2:DescribeInstances", "ec2:DescribeVolumes", "ec2:DescribeSnapshots"]
        Resource = "*"
        Condition = {
          StringEquals = { "aws:RequestedRegion" = var.region }
        }
      },
      {
        Effect = "Allow"
        Action = "ec2:CreateSnapshots"
        Resource = [
          aws_instance.desktop.arn,
          "arn:${data.aws_partition.backup.partition}:ec2:${var.region}:${data.aws_caller_identity.backup[0].account_id}:volume/${aws_instance.desktop.root_block_device[0].volume_id}"
        ]
      },
      {
        Effect   = "Allow"
        Action   = "ec2:CreateSnapshots"
        Resource = local.backup_snapshot_arn
        Condition = {
          StringEquals = { "aws:RequestTag/NeoAgentBackup" = local.name }
        }
      },
      {
        Effect   = "Allow"
        Action   = "ec2:CreateTags"
        Resource = local.backup_snapshot_arn
        Condition = {
          StringEquals = {
            "ec2:CreateAction"              = "CreateSnapshots"
            "aws:RequestTag/NeoAgentBackup" = local.name
          }
        }
      },
      {
        Effect   = "Allow"
        Action   = "ec2:DeleteSnapshot"
        Resource = local.backup_snapshot_arn
        Condition = {
          StringEquals = { "aws:ResourceTag/NeoAgentBackup" = local.name }
          StringLike   = { "aws:ResourceTag/aws:dlm:lifecycle-policy-id" = "policy-*" }
        }
      },
      {
        Effect = "Allow"
        Action = [
          "events:PutRule", "events:DeleteRule", "events:DescribeRule", "events:EnableRule",
          "events:DisableRule", "events:ListTargetsByRule", "events:PutTargets", "events:RemoveTargets"
        ]
        Resource = "arn:${data.aws_partition.backup.partition}:events:${var.region}:${data.aws_caller_identity.backup[0].account_id}:rule/AwsDataLifecycleRule.managed-cwe.*"
      },
      {
        Effect   = "Allow"
        Action   = ["kms:DescribeKey", "kms:Decrypt", "kms:GenerateDataKeyWithoutPlaintext", "kms:ReEncrypt*"]
        Resource = aws_kms_key.desktop.arn
        Condition = {
          StringEquals = { "kms:ViaService" = "ec2.${var.region}.${data.aws_partition.backup.dns_suffix}" }
        }
      },
      {
        Effect   = "Allow"
        Action   = "kms:CreateGrant"
        Resource = aws_kms_key.desktop.arn
        Condition = {
          Bool = { "kms:GrantIsForAWSResource" = "true" }
        }
      }
    ]
  })
}

resource "aws_dlm_lifecycle_policy" "desktop_backup" {
  count              = var.enable_desktop_backups ? 1 : 0
  description        = "Daily encrypted root-volume snapshots of ${local.name}"
  execution_role_arn = aws_iam_role.desktop_backup[0].arn
  state              = "ENABLED"

  policy_details {
    policy_type    = "EBS_SNAPSHOT_MANAGEMENT"
    resource_types = ["INSTANCE"]
    target_tags    = local.desktop_backup_tags

    schedule {
      name        = "daily"
      tags_to_add = local.desktop_backup_tags

      create_rule {
        interval      = 24
        interval_unit = "HOURS"
        times         = ["03:00"]
      }

      retain_rule {
        count = 7
      }
    }
  }

  depends_on = [aws_iam_role_policy.desktop_backup]
}

output "desktop_backup_policy_id" {
  description = "Data Lifecycle Manager policy for desktop snapshots when enabled."
  value       = one(aws_dlm_lifecycle_policy.desktop_backup[*].id)
}
