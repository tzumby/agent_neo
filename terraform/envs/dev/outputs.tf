output "vpc_id" {
  value = module.network.vpc_id
}

output "azs" {
  value = module.network.azs
}

output "public_subnet_ids" {
  value = module.network.public_subnet_ids
}

output "private_subnet_ids" {
  value = module.network.private_subnet_ids
}

output "desktop_sg_id" {
  value = module.security.desktop_sg_id
}

output "control_plane_sg_id" {
  value = module.security.control_plane_sg_id
}

output "desktop_instance_profile" {
  value = module.iam.desktop_instance_profile_name
}

output "ssm_transfer_bucket" {
  description = "Bucket for Ansible's aws_ssm connection (ansible_aws_ssm_bucket_name)."
  value       = module.desktop_fleet.ssm_transfer_bucket
}

output "ubuntu_ami" {
  value = "${data.aws_ami.ubuntu.id} (${data.aws_ami.ubuntu.name})"
}

output "test_desktop_instance_id" {
  value = one(aws_instance.test_desktop[*].id)
}

output "desktop_instance_ids" {
  description = "Every desktop's instance, by user (the test desktop under test_desktop_user)."
  value = merge(
    var.create_test_desktop ? { (var.test_desktop_user) = aws_instance.test_desktop[0].id } : {},
    { for user, desktop in aws_instance.desktop : user => desktop.id },
  )
}

output "push_relay_url" {
  value = module.push_relay.relay_url
}

output "user_kms_keys" {
  value = { for user, key in aws_kms_key.user : user => key.arn }
}

output "test_desktop_role" {
  value = one(module.test_desktop_identity[*].role_name)
}

output "image_builder" {
  value = var.enable_image_builder ? {
    instance_profile  = module.image_builder[0].instance_profile_name
    security_group_id = module.image_builder[0].security_group_id
    subnet_id         = module.network.private_subnet_ids[0]
  } : null
}

output "attested_gateway" {
  value = {
    url         = module.attested_gateway.url
    instance_id = module.attested_gateway.instance_id
  }
}
