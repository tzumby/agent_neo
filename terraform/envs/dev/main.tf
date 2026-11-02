locals {
  name = "${var.project}-${var.env}"

  # First two AZs (sorted) where the desktop instance type is offered.
  azs = slice(sort(data.aws_ec2_instance_type_offerings.desktop.locations), 0, 2)
}

data "aws_ec2_instance_type_offerings" "desktop" {
  location_type = "availability-zone"

  filter {
    name   = "instance-type"
    values = [var.desktop_instance_type]
  }
}

module "network" {
  source = "../../modules/network"

  name                 = local.name
  vpc_cidr             = var.vpc_cidr
  azs                  = local.azs
  enable_nat_gateway   = var.enable_nat_gateway
  nat_per_az           = var.nat_per_az
  enable_nat_instance  = var.enable_nat_instance
  enable_ssm_endpoints = var.enable_ssm_endpoints
}

module "security" {
  source = "../../modules/security"

  name     = local.name
  vpc_id   = module.network.vpc_id
  app_port = var.app_port
}

module "desktop_fleet" {
  source = "../../modules/desktop_fleet"

  name = local.name
}

module "iam" {
  source = "../../modules/iam"

  name                    = local.name
  ssm_transfer_bucket_arn = module.desktop_fleet.ssm_transfer_bucket_arn
  ssm_parameter_prefix    = "/${var.project}"
}

locals {
  # The fleet role, and each desktop's own role (<fleet role>-<user>).
  desktop_role_prefix = module.iam.desktop_role_name
  desktop_role_names = concat(
    [module.iam.desktop_role_name],
    module.test_desktop_identity[*].role_name,
    [for identity in values(module.desktop_identity) : identity.role_name],
  )
}

# Push notifications to the iOS app. The APNs key stays in SSM and this
# relay; desktops only ask it to notify their owner.
module "push_relay" {
  source = "../../modules/push_relay"

  name                = local.name
  desktop_role_prefix = local.desktop_role_prefix
  desktop_role_names  = local.desktop_role_names
  ssm_prefix          = "/${var.project}/${var.env}"
  apns_team_id        = var.apns_team_id
  apns_topic          = var.apns_topic
  apns_environment    = var.apns_environment
  source_dir          = "${path.root}/../../../services/push-relay/src"
}

# The attested model gateway (docs/operator-isolation.md): gatewayd on an
# image nobody can log into. Desktops send prompts only after checking its
# measurements, which the image build published for that image.
module "image_builder" {
  source = "../../modules/image_builder"
  count  = var.enable_image_builder ? 1 : 0

  name       = local.name
  vpc_id     = module.network.vpc_id
  ssm_prefix = "/${var.project}/${var.env}"
}

data "aws_ssm_parameter" "gateway_measurements" {
  count = var.gateway_ami != "" ? 1 : 0
  name  = "/${var.project}/${var.env}/gateway/images/${var.gateway_ami}"
}

# The Lambda model gateway is retired; its usage table lives on here.
moved {
  from = module.model_gateway.aws_dynamodb_table.usage
  to   = module.attested_gateway.aws_dynamodb_table.usage
}

module "attested_gateway" {
  source = "../../modules/attested_gateway"

  name                = local.name
  ami                 = var.gateway_ami
  measurements        = var.gateway_ami != "" ? data.aws_ssm_parameter.gateway_measurements[0].value : ""
  vpc_id              = module.network.vpc_id
  subnet_id           = module.network.private_subnet_ids[0]
  private_ip          = cidrhost(module.network.private_subnet_cidrs[0], 10)
  desktop_sg_id       = module.security.desktop_sg_id
  desktop_role_prefix = local.desktop_role_prefix
  audience            = "${local.name}/model-gateway"
  ssm_prefix          = "/${var.project}/${var.env}"
  daily_budget_usd    = var.model_daily_budget_usd
  prices = {
    "zai-org/GLM-5.3"       = [1.40, 0.26, 4.40]
    "zai-org/GLM-5.3-Flash" = [0.15, 0.03, 0.50]
    "moonshotai/Kimi-K3"    = [3.00, 0.30, 15.00]
  }
}

# One encryption key per user for their desktop's disks and snapshots.
# Deleting a user's key makes their data unreadable (crypto-shredding).
resource "aws_kms_key" "user" {
  for_each = toset(var.users)

  description             = "Disks and snapshots of ${each.key}'s desktop"
  enable_key_rotation     = true
  deletion_window_in_days = 30

  tags = {
    User = each.key
  }
}

resource "aws_kms_alias" "user" {
  for_each = toset(var.users)

  name          = "alias/${local.name}/user/${each.key}"
  target_key_id = aws_kms_key.user[each.key].key_id
}

# The test desktop's own identity (its user's desktop role).
module "test_desktop_identity" {
  source = "../../modules/desktop_identity"
  count  = var.create_test_desktop ? 1 : 0

  role_name            = "${module.iam.desktop_role_name}-${var.test_desktop_user}"
  user                 = var.test_desktop_user
  ssm_parameter_prefix = "/${var.project}"
}

# --- Test desktop ------------------------------------------------------------
# A single hand-managed desktop for developing the Ansible role. Real desktops
# are launched by the control plane from a launch template, not by Terraform.

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-*-${var.ubuntu_version}-${var.desktop_arch}-server-*"]
  }

  filter {
    name   = "architecture"
    values = [var.desktop_arch == "amd64" ? "x86_64" : "arm64"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

# Changing the architecture means a new desktop from a new image (the AMI
# itself is ignored below, so new Ubuntu releases don't replace it).
resource "terraform_data" "desktop_arch" {
  input = var.desktop_arch
}

resource "aws_instance" "test_desktop" {
  count = var.create_test_desktop ? 1 : 0

  ami           = data.aws_ami.ubuntu.id
  instance_type = var.desktop_instance_type
  # Private subnet, no public IP: out through the NAT, in through nothing.
  subnet_id                   = module.network.private_subnet_ids[0]
  associate_public_ip_address = false
  vpc_security_group_ids      = [module.security.desktop_sg_id]
  iam_instance_profile        = module.test_desktop_identity[0].instance_profile_name

  credit_specification {
    cpu_credits = var.cpu_credits
  }

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_type = "gp3"
    volume_size = 20
    encrypted   = true
    # The user's own key.
    kms_key_id            = aws_kms_key.user[var.test_desktop_user].arn
    delete_on_termination = true
  }

  tags = {
    Name = "${local.name}-test-desktop"
    Role = "desktop"
    # For Ansible (inventory/aws_ec2.yml): whose desktop, and on which tailnet.
    User        = var.test_desktop_user
    Tailnet     = "shared" # the operator's tailnet, its auth key and allowlist
    TailnetName = "cloud-desktop-dev"
  }

  lifecycle {
    # Don't replace the instance whenever Canonical publishes a new AMI.
    ignore_changes       = [ami]
    replace_triggered_by = [terraform_data.desktop_arch]

    precondition {
      condition     = var.enable_nat_gateway || var.enable_nat_instance
      error_message = "Desktops have no public IP: enable_nat_gateway or enable_nat_instance must be on for them to reach the internet."
    }
  }
}

# --- Other desktops (a tester, later users) ----------------------------------
# One per entry in var.desktops, set up like the test desktop: its own role,
# its user's own encryption key, a private subnet, no inbound. It joins the
# desktops tailnet (Tailnet = "desktops"), tagged tag:desktop-<name>, where
# only devices tagged tag:user-<name> reach it (scripts/tailnet.py); never
# the operator's personal tailnet. Added and set up with scripts/new-desktop.sh.

module "desktop_identity" {
  source   = "../../modules/desktop_identity"
  for_each = var.desktops

  role_name            = "${module.iam.desktop_role_name}-${each.key}"
  user                 = each.key
  ssm_parameter_prefix = "/${var.project}"
}

resource "aws_instance" "desktop" {
  for_each = var.desktops

  ami                         = data.aws_ami.ubuntu.id
  instance_type               = coalesce(each.value.instance_type, var.desktop_instance_type)
  subnet_id                   = module.network.private_subnet_ids[0]
  associate_public_ip_address = false
  vpc_security_group_ids      = [module.security.desktop_sg_id]
  iam_instance_profile        = module.desktop_identity[each.key].instance_profile_name

  credit_specification {
    cpu_credits = var.cpu_credits
  }

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_type           = "gp3"
    volume_size           = 20
    encrypted             = true
    kms_key_id            = aws_kms_key.user[each.key].arn
    delete_on_termination = true
  }

  tags = {
    Name        = "${local.name}-desktop-${each.key}"
    Role        = "desktop"
    User        = each.key
    Tailnet     = "desktops"
    TailnetName = "cloud-desktop-${each.key}"
  }

  lifecycle {
    ignore_changes       = [ami]
    replace_triggered_by = [terraform_data.desktop_arch]

    precondition {
      condition     = var.enable_nat_gateway || var.enable_nat_instance
      error_message = "Desktops have no public IP: enable_nat_gateway or enable_nat_instance must be on for them to reach the internet."
    }
  }
}
