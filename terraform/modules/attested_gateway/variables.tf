variable "name" {
  description = "Name prefix, e.g. cloud-desktops-dev."
  type        = string
}

variable "ami" {
  description = "A gateway image (scripts/build-gateway-image.sh). Empty: no gateway instance."
  type        = string
  default     = ""
}

variable "attested" {
  description = "Desktops check the gateway's NitroTPM attestation against var.measurements (needs a type that can attest: c7g.medium). False: they pin the certificate it publishes to var.certificate_parameter."
  type        = bool
  default     = false
}

variable "measurements" {
  description = "The image's boot measurements, \"4=<hex>,12=<hex>\" (published by the image build). With attested, desktops accept only these."
  type        = string
  default     = ""
}

variable "certificate_parameter" {
  description = "SSM parameter gatewayd publishes its certificate's fingerprint to. Outside the project prefix desktops are denied, e.g. /cloud-desktops-shared/<env>/gateway/certificate."
  type        = string
}

variable "instance_type" {
  description = "Null: t4g.micro, or c7g.medium when attested (burstable types' NitroTPM holds 2 KiB per NV index and nitro-tpm-attest needs 8 KiB: aws/NitroTPM-Tools#7)."
  type        = string
  default     = null
}

variable "vpc_id" {
  type = string
}

variable "subnet_id" {
  type = string
}

variable "private_ip" {
  description = "Fixed address, so desktops keep the same URL across image updates."
  type        = string
}

variable "desktop_sg_id" {
  description = "Desktops' security group: the only callers."
  type        = string
}

variable "desktop_role_prefix" {
  type = string
}

variable "audience" {
  description = "Signed into desktops' identity proofs for this gateway."
  type        = string
}

variable "ssm_prefix" {
  type = string
}

variable "prices" {
  description = "Model → [input, cached input, output] USD per 1M tokens; only these models are allowed."
  type        = map(list(number))
}

variable "daily_budget_usd" {
  type = number
}

variable "permissions_boundary" {
  description = "Permissions boundary for the role (a customer deployment's: deploy/customer-aws). Null: none."
  type        = string
  default     = null
}
