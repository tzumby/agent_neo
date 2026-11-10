output "desktop_instance_id" {
  description = "The desktop's instance (make shell, viewer, cdp, agentd-logs)."
  value       = aws_instance.desktop.id
}

output "ssm_transfer_bucket" {
  description = "Bucket for Ansible's aws_ssm connection (ansible_aws_ssm_bucket_name)."
  value       = module.desktop_fleet.ssm_transfer_bucket
}

output "tailnet_hostname" {
  description = "The desktop's device name on your tailnet: https://<this>.<your tailnet>.ts.net."
  value       = var.tailnet_hostname
}

output "nat_public_ips" {
  description = "Where the desktop's traffic comes from on the internet (to allowlist it somewhere)."
  value       = module.network.nat_public_ips
}

output "ubuntu_ami" {
  value = "${data.aws_ami.ubuntu.id} (${data.aws_ami.ubuntu.name})"
}

output "desktop_kms_key" {
  value = aws_kms_key.desktop.arn
}
