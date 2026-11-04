# A customer deployment (docs/customer-cloud.md): the agent desktop(s) and the
# model gateway in the customer's own AWS account, a sandbox with nothing else
# of theirs in it. Nothing connects it to their other networks; people reach
# the desktop through Cloudflare Access and Tunnel (front-door), and the
# desktop reaches the internet through the NAT's fixed address.
#
# Every role gets the customer's permissions boundary, the only kind of role
# our deployer may create (deploy/customer-aws). No Tailscale, no push relay.

locals {
  name = "cloud-desktops-${var.env}"

  # Desktop roles are <prefix>-<name>; the gateway accepts callers with it.
  desktop_role_prefix = "${local.name}-desktop"

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

  name     = local.name
  vpc_cidr = var.vpc_cidr
  azs      = local.azs
  # AWS's NAT gateway (about $33/month), not the fck-nat instance our dev
  # account uses: no third-party image in the customer's account, and no
  # instance with a public address (the guardrails deny those).
  enable_nat_gateway = true
}

# Desktops take no inbound connections at all: cloudflared dials out.
resource "aws_security_group" "desktop" {
  name        = "${local.name}-desktop"
  description = "Agent desktops: nothing in; out to the internet through the NAT"
  vpc_id      = module.network.vpc_id

  tags = {
    Name = "${local.name}-desktop"
  }
}

resource "aws_vpc_security_group_egress_rule" "desktop_all" {
  security_group_id = aws_security_group.desktop.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
}

# The bucket Ansible's SSM connection moves files through.
module "desktop_fleet" {
  source = "../../modules/desktop_fleet"

  name = local.name
}

# --- The model gateway ---------------------------------------------------------
# Holds the customer's Together key (they add it to SSM after our access is
# off; gatewayd retries until it's there), so it never reaches the desktop.
# Desktops pin the certificate it publishes to SSM: in the customer's own
# account, its admins are the only ones who could change that, and attesting
# wouldn't keep them out of their own desktop anyway (gateway_attested).

module "image_builder" {
  source = "../../modules/image_builder"
  count  = var.enable_image_builder ? 1 : 0

  name                 = local.name
  vpc_id               = module.network.vpc_id
  ssm_prefix           = "/cloud-desktops/${var.env}"
  permissions_boundary = var.permissions_boundary
}

data "aws_ssm_parameter" "gateway_measurements" {
  count = var.gateway_ami != "" && var.gateway_attested ? 1 : 0
  name  = "/cloud-desktops/${var.env}/gateway/images/${var.gateway_ami}"
}

module "attested_gateway" {
  source = "../../modules/attested_gateway"

  name                  = local.name
  ami                   = var.gateway_ami
  attested              = var.gateway_attested
  measurements          = one(data.aws_ssm_parameter.gateway_measurements[*].value)
  certificate_parameter = "/cloud-desktops-shared/${var.env}/gateway/certificate"
  vpc_id                = module.network.vpc_id
  subnet_id             = module.network.private_subnet_ids[0]
  private_ip            = cidrhost(module.network.private_subnet_cidrs[0], 10)
  desktop_sg_id         = aws_security_group.desktop.id
  desktop_role_prefix   = local.desktop_role_prefix
  audience              = "${local.name}/model-gateway"
  ssm_prefix            = "/cloud-desktops/${var.env}"
  daily_budget_usd      = var.model_daily_budget_usd
  prices                = var.model_prices
  permissions_boundary  = var.permissions_boundary
}

# --- Desktops ------------------------------------------------------------------

# Each desktop's disk key, in the customer's account (the default key policy:
# their account's admins control it). Deleting it makes the disk unreadable.
resource "aws_kms_key" "desktop" {
  for_each = var.desktops

  description             = "Disk of the ${each.key} agent desktop"
  enable_key_rotation     = true
  deletion_window_in_days = 30

  tags = {
    User = each.key
  }
}

resource "aws_kms_alias" "desktop" {
  for_each = var.desktops

  name          = "alias/${local.name}/desktop/${each.key}"
  target_key_id = aws_kms_key.desktop[each.key].key_id
}

# Its own role: SSM for Ansible while we deploy, never the project's
# parameters, and only its own front-door settings.
module "desktop_identity" {
  source   = "../../modules/desktop_identity"
  for_each = var.desktops

  role_name            = "${local.desktop_role_prefix}-${each.key}"
  user                 = each.key
  ssm_parameter_prefix = "/cloud-desktops"
  own_parameters_path  = "/front-door/${var.env}/${each.key}"
  permissions_boundary = var.permissions_boundary
}

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

resource "aws_instance" "desktop" {
  for_each = var.desktops

  ami           = data.aws_ami.ubuntu.id
  instance_type = coalesce(each.value.instance_type, var.desktop_instance_type)
  # Private subnet, no public IP: out through the NAT, in through nothing.
  subnet_id                   = module.network.private_subnet_ids[0]
  associate_public_ip_address = false
  vpc_security_group_ids      = [aws_security_group.desktop.id]
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
    kms_key_id            = aws_kms_key.desktop[each.key].arn
    delete_on_termination = true
  }

  tags = {
    Name = "${local.name}-desktop-${each.key}"
    Role = "desktop"
    # For Ansible (inventory/aws_ec2.yml): whose desktop, its name, and that
    # people reach it through Cloudflare (front-door), not a tailnet.
    User        = each.key
    DesktopName = "${var.env}-${each.key}"
    Access      = "cloudflare"
  }

  lifecycle {
    # Don't replace the instance whenever Canonical publishes a new AMI.
    ignore_changes = [ami]
  }
}
