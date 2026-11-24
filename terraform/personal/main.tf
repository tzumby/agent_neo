# A personal deployment: one agent desktop in your own AWS account. It sits
# in a private subnet with no public IP and no inbound rules, reaches the
# internet through a NAT, and joins your tailnet outbound (tailnet-proxy),
# which is how you reach it. Administration is SSM Session Manager only.

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
  source = "../modules/network"

  name                 = local.name
  vpc_cidr             = var.vpc_cidr
  azs                  = local.azs
  enable_nat_gateway   = var.enable_nat_gateway
  enable_nat_instance  = var.enable_nat_instance
  enable_ssm_endpoints = var.enable_ssm_endpoints
}

# The desktop takes no inbound connections at all: tailnet-proxy dials out to
# Tailscale, and SSM's agent dials out to AWS.
resource "aws_security_group" "desktop" {
  name        = "${local.name}-desktop"
  description = "Agent desktop: nothing in; out to the internet through the NAT"
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
  source = "../modules/desktop_fleet"

  name = local.name
}

# The desktop's disk key. Deleting it makes the disk and its snapshots
# unreadable.
resource "aws_kms_key" "desktop" {
  description             = "Disk of the ${local.name} agent desktop"
  enable_key_rotation     = true
  deletion_window_in_days = 30
}

resource "aws_kms_alias" "desktop" {
  name          = "alias/${local.name}/desktop"
  target_key_id = aws_kms_key.desktop.key_id
}

# Its own role: SSM for Ansible and Session Manager, never the project's
# parameters (your model key and Tailscale auth key, which Ansible reads on
# your machine and writes to the desktop).
module "desktop_identity" {
  source = "../modules/desktop_identity"

  role_name            = "${local.name}-desktop"
  ssm_parameter_prefix = "/${var.project}"
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

# Changing the architecture means a new desktop from a new image (the AMI
# itself is ignored below, so new Ubuntu releases don't replace it).
resource "terraform_data" "desktop_arch" {
  input = var.desktop_arch
}

resource "aws_instance" "desktop" {
  ami           = data.aws_ami.ubuntu.id
  instance_type = var.desktop_instance_type
  # Private subnet, no public IP: out through the NAT, in through nothing.
  subnet_id                   = module.network.private_subnet_ids[0]
  associate_public_ip_address = false
  vpc_security_group_ids      = [aws_security_group.desktop.id]
  iam_instance_profile        = module.desktop_identity.instance_profile_name

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
    volume_size           = var.disk_size_gb
    encrypted             = true
    kms_key_id            = aws_kms_key.desktop.arn
    delete_on_termination = true
  }

  tags = merge({
    Name = "${local.name}-desktop"
    # For Ansible (ansible/inventory/aws_ec2.yml.tmpl): what it is, and its
    # device name on your tailnet.
    Role        = "desktop"
    TailnetName = var.tailnet_hostname
  }, var.enable_desktop_backups ? local.desktop_backup_tags : {})

  lifecycle {
    # Don't replace the instance whenever Canonical publishes a new AMI.
    ignore_changes       = [ami]
    replace_triggered_by = [terraform_data.desktop_arch]

    precondition {
      condition     = var.enable_nat_gateway || var.enable_nat_instance
      error_message = "The desktop has no public IP: enable_nat_instance or enable_nat_gateway must be on for it to reach the internet."
    }
  }
}
