data "aws_region" "current" {}

locals {
  nat_count = var.enable_nat_gateway ? (var.nat_per_az ? length(var.azs) : 1) : 0
}

resource "aws_vpc" "this" {
  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true

  tags = {
    Name = var.name
  }
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id

  tags = {
    Name = var.name
  }
}

# --- Public subnets: the NAT (instance or gateways). /24 each. ---------------

resource "aws_subnet" "public" {
  count = length(var.azs)

  vpc_id                  = aws_vpc.this.id
  availability_zone       = var.azs[count.index]
  cidr_block              = cidrsubnet(var.vpc_cidr, 8, count.index)
  map_public_ip_on_launch = false

  tags = {
    Name = "${var.name}-public-${var.azs[count.index]}"
    Tier = "public"
  }
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id

  tags = {
    Name = "${var.name}-public"
  }
}

resource "aws_route" "public_internet" {
  route_table_id         = aws_route_table.public.id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = aws_internet_gateway.this.id
}

resource "aws_route_table_association" "public" {
  count = length(var.azs)

  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

# --- Private subnets: the desktop. /20 each. ---------------------------------

resource "aws_subnet" "private" {
  count = length(var.azs)

  vpc_id            = aws_vpc.this.id
  availability_zone = var.azs[count.index]
  cidr_block        = cidrsubnet(var.vpc_cidr, 4, count.index + 1)

  tags = {
    Name = "${var.name}-private-${var.azs[count.index]}"
    Tier = "private"
  }
}

resource "aws_eip" "nat" {
  count = local.nat_count

  domain = "vpc"

  tags = {
    Name = "${var.name}-nat-${var.azs[count.index]}"
  }
}

resource "aws_nat_gateway" "this" {
  count = local.nat_count

  allocation_id = aws_eip.nat[count.index].id
  subnet_id     = aws_subnet.public[count.index].id

  tags = {
    Name = "${var.name}-${var.azs[count.index]}"
  }

  depends_on = [aws_internet_gateway.this]
}

resource "aws_route_table" "private" {
  count = length(var.azs)

  vpc_id = aws_vpc.this.id

  tags = {
    Name = "${var.name}-private-${var.azs[count.index]}"
  }
}

resource "aws_route" "private_nat" {
  count = var.enable_nat_gateway ? length(var.azs) : 0

  route_table_id         = aws_route_table.private[count.index].id
  destination_cidr_block = "0.0.0.0/0"
  nat_gateway_id         = aws_nat_gateway.this[var.nat_per_az ? count.index : 0].id
}

# --- NAT instance (fck-nat) ---------------------------------------------------
# A cheaper alternative to managed NAT gateways: one t4g.nano running the
# fck-nat AMI (https://fck-nat.dev), in the first public subnet with its own
# Elastic IP. Not highly available; fine until the product needs more.

data "aws_ami" "fck_nat" {
  count = var.enable_nat_instance ? 1 : 0

  most_recent = true
  owners      = ["568608671756"] # fck-nat

  filter {
    name   = "name"
    values = ["fck-nat-al2023-*"]
  }

  filter {
    name   = "architecture"
    values = ["arm64"]
  }
}

resource "aws_security_group" "nat" {
  count = var.enable_nat_instance ? 1 : 0

  name        = "${var.name}-nat"
  description = "NAT instance: forwards traffic from the VPC to the internet"
  vpc_id      = aws_vpc.this.id

  tags = {
    Name = "${var.name}-nat"
  }
}

resource "aws_vpc_security_group_ingress_rule" "nat_from_vpc" {
  count = var.enable_nat_instance ? 1 : 0

  security_group_id = aws_security_group.nat[0].id
  description       = "Anything from inside the VPC, to forward"
  ip_protocol       = "-1"
  cidr_ipv4         = var.vpc_cidr
}

resource "aws_vpc_security_group_egress_rule" "nat_out" {
  count = var.enable_nat_instance ? 1 : 0

  security_group_id = aws_security_group.nat[0].id
  description       = "To the internet"
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_network_interface" "nat" {
  count = var.enable_nat_instance ? 1 : 0

  subnet_id         = aws_subnet.public[0].id
  security_groups   = [aws_security_group.nat[0].id]
  source_dest_check = false

  tags = {
    Name = "${var.name}-nat"
  }
}

resource "aws_eip" "nat_instance" {
  count = var.enable_nat_instance ? 1 : 0

  domain            = "vpc"
  network_interface = aws_network_interface.nat[0].id

  tags = {
    Name = "${var.name}-nat"
  }

  depends_on = [aws_internet_gateway.this]
}

resource "aws_instance" "nat" {
  count = var.enable_nat_instance ? 1 : 0

  ami           = data.aws_ami.fck_nat[0].id
  instance_type = var.nat_instance_type

  primary_network_interface {
    network_interface_id = aws_network_interface.nat[0].id
  }

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_type = "gp3"
    encrypted   = true
  }

  tags = {
    Name = "${var.name}-nat"
    Role = "nat"
  }

  lifecycle {
    # A new fck-nat release shouldn't replace the NAT under running desktops.
    ignore_changes = [ami]
  }
}

resource "aws_route" "private_nat_instance" {
  count = var.enable_nat_instance ? length(var.azs) : 0

  route_table_id         = aws_route_table.private[count.index].id
  destination_cidr_block = "0.0.0.0/0"
  network_interface_id   = aws_network_interface.nat[0].id
}

resource "aws_route_table_association" "private" {
  count = length(var.azs)

  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private[count.index].id
}

# --- VPC endpoints -----------------------------------------------------------

resource "aws_vpc_endpoint" "s3" {
  vpc_id            = aws_vpc.this.id
  service_name      = "com.amazonaws.${data.aws_region.current.region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = concat(aws_route_table.private[*].id, [aws_route_table.public.id])

  tags = {
    Name = "${var.name}-s3"
  }
}

resource "aws_security_group" "endpoints" {
  count = var.enable_ssm_endpoints ? 1 : 0

  name        = "${var.name}-vpce"
  description = "HTTPS from inside the VPC to interface endpoints"
  vpc_id      = aws_vpc.this.id

  tags = {
    Name = "${var.name}-vpce"
  }
}

resource "aws_vpc_security_group_ingress_rule" "endpoints_https" {
  count = var.enable_ssm_endpoints ? 1 : 0

  security_group_id = aws_security_group.endpoints[0].id
  cidr_ipv4         = var.vpc_cidr
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
}

resource "aws_vpc_endpoint" "ssm" {
  for_each = var.enable_ssm_endpoints ? toset(["ssm", "ssmmessages", "ec2messages"]) : toset([])

  vpc_id              = aws_vpc.this.id
  service_name        = "com.amazonaws.${data.aws_region.current.region}.${each.key}"
  vpc_endpoint_type   = "Interface"
  subnet_ids          = aws_subnet.private[*].id
  security_group_ids  = [aws_security_group.endpoints[0].id]
  private_dns_enabled = true

  tags = {
    Name = "${var.name}-${each.key}"
  }
}
