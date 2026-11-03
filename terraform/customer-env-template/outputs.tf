# What the Makefile and scripts read (make desktop, build-gateway-image.sh),
# and what the runbook hands the customer.

output "ssm_transfer_bucket" {
  value = module.customer.ssm_transfer_bucket
}

output "desktop_instance_ids" {
  value = module.customer.desktop_instance_ids
}

output "image_builder" {
  value = module.customer.image_builder
}

output "attested_gateway" {
  value = module.customer.attested_gateway
}

output "nat_public_ips" {
  value = module.customer.nat_public_ips
}

output "front_door_parameters" {
  value = module.customer.front_door_parameters
}

output "model_key_parameter" {
  value = module.customer.model_key_parameter
}
