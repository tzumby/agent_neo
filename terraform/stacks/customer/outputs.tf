output "vpc_id" {
  value = module.network.vpc_id
}

output "nat_public_ips" {
  description = "The desktops' fixed address on the internet: what the customer allowlists in Cloudflare for the agent's testing."
  value       = module.network.nat_public_ips
}

output "ssm_transfer_bucket" {
  value = module.desktop_fleet.ssm_transfer_bucket
}

output "ubuntu_ami" {
  value = data.aws_ami.ubuntu.id
}

output "desktop_instance_ids" {
  description = "Desktop name → instance id (make … DESKTOP=<name>)."
  value       = { for name, instance in aws_instance.desktop : name => instance.id }
}

output "front_door_parameters" {
  description = "Desktop name → the SSM path the customer puts its Cloudflare settings under (docs/customer-cloud.md)."
  value       = { for name in keys(var.desktops) : name => "/front-door/${var.env}/${name}" }
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

output "model_key_parameter" {
  description = "Where the customer puts their Together API key (SecureString)."
  value       = "/cloud-desktops/${var.env}/llm/together_api_key"
}
