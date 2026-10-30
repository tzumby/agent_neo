locals {
  desktop_ports = {
    agent = var.desktop_agent_port
    novnc = var.novnc_port
  }
}

# --- ALB ---------------------------------------------------------------------

resource "aws_security_group" "alb" {
  name        = "${var.name}-alb"
  description = "Public ALB in front of the control plane"
  vpc_id      = var.vpc_id

  tags = {
    Name = "${var.name}-alb"
  }
}

resource "aws_vpc_security_group_ingress_rule" "alb" {
  for_each = toset(["80", "443"]) # 80 only redirects to 443

  security_group_id = aws_security_group.alb.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "tcp"
  from_port         = tonumber(each.key)
  to_port           = tonumber(each.key)
}

resource "aws_vpc_security_group_egress_rule" "alb_to_control_plane" {
  security_group_id            = aws_security_group.alb.id
  referenced_security_group_id = aws_security_group.control_plane.id
  ip_protocol                  = "tcp"
  from_port                    = var.app_port
  to_port                      = var.app_port
}

# --- Control plane -----------------------------------------------------------

resource "aws_security_group" "control_plane" {
  name        = "${var.name}-control-plane"
  description = "Control plane: app port from the ALB only"
  vpc_id      = var.vpc_id

  tags = {
    Name = "${var.name}-control-plane"
  }
}

resource "aws_vpc_security_group_ingress_rule" "control_plane_from_alb" {
  security_group_id            = aws_security_group.control_plane.id
  referenced_security_group_id = aws_security_group.alb.id
  ip_protocol                  = "tcp"
  from_port                    = var.app_port
  to_port                      = var.app_port
}

resource "aws_vpc_security_group_egress_rule" "control_plane_all" {
  security_group_id = aws_security_group.control_plane.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
}

# --- Desktops ----------------------------------------------------------------
# Inbound only from the control plane. No self-referencing rule, so desktops
# cannot reach each other.

resource "aws_security_group" "desktop" {
  name        = "${var.name}-desktop"
  description = "Desktops: agent daemon and noVNC from the control plane only"
  vpc_id      = var.vpc_id

  tags = {
    Name = "${var.name}-desktop"
  }
}

resource "aws_vpc_security_group_ingress_rule" "desktop_from_control_plane" {
  for_each = local.desktop_ports

  security_group_id            = aws_security_group.desktop.id
  referenced_security_group_id = aws_security_group.control_plane.id
  ip_protocol                  = "tcp"
  from_port                    = each.value
  to_port                      = each.value
  description                  = each.key
}

resource "aws_vpc_security_group_egress_rule" "desktop_all" {
  security_group_id = aws_security_group.desktop.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
}
