output "url" {
  value = local.enabled ? "https://${var.private_ip}:8443" : ""
}

output "instance_id" {
  value = local.enabled ? aws_instance.gateway[0].id : ""
}
