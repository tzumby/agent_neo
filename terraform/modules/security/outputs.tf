output "alb_sg_id" {
  value = aws_security_group.alb.id
}

output "control_plane_sg_id" {
  value = aws_security_group.control_plane.id
}

output "desktop_sg_id" {
  value = aws_security_group.desktop.id
}
