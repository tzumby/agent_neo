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

# Push notifications to the iOS app. The APNs key stays in SSM and this
# relay; desktops only ask it to notify their owner.
module "push_relay" {
  source = "../../modules/push_relay"

  name              = local.name
  desktop_role_name = module.iam.desktop_role_name
  ssm_prefix        = "/${var.project}/${var.env}"
  apns_team_id      = var.apns_team_id
  apns_topic        = var.apns_topic
  apns_environment  = var.apns_environment
  source_dir        = "${path.root}/../../../services/push-relay/src"
}

# --- Test desktop ------------------------------------------------------------
# A single hand-managed desktop for developing the Ansible role. Real desktops
# are launched by the control plane from a launch template, not by Terraform.

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-*-${var.ubuntu_version}-arm64-server-*"]
  }

  filter {
    name   = "architecture"
    values = ["arm64"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

resource "aws_instance" "test_desktop" {
  count = var.create_test_desktop ? 1 : 0

  ami                         = data.aws_ami.ubuntu.id
  instance_type               = var.desktop_instance_type
  subnet_id                   = var.test_desktop_public_ip ? module.network.public_subnet_ids[0] : module.network.private_subnet_ids[0]
  associate_public_ip_address = var.test_desktop_public_ip
  vpc_security_group_ids      = [module.security.desktop_sg_id]
  iam_instance_profile        = module.iam.desktop_instance_profile_name

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
    delete_on_termination = true
  }

  tags = {
    Name = "${local.name}-test-desktop"
    Role = "desktop"
  }

  lifecycle {
    # Don't replace the instance whenever Canonical publishes a new AMI.
    ignore_changes = [ami]

    precondition {
      condition     = var.test_desktop_public_ip || var.enable_nat_gateway
      error_message = "A test desktop without a public IP needs enable_nat_gateway = true to reach the internet."
    }
  }
}
