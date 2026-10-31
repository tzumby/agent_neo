variable "name" {
  description = "Name prefix, e.g. cloud-desktops-dev."
  type        = string
}

variable "ami" {
  description = "An attested gateway image (scripts/build-gateway-image.sh). Empty: no gateway instance."
  type        = string
  default     = ""
}

variable "measurements" {
  description = "The image's boot measurements, \"4=<hex>,12=<hex>\" (published by the image build). Desktops accept only these."
  type        = string
  default     = ""
}

variable "instance_type" {
  description = "Not burstable (t3/t4g): their NitroTPM holds 2 KiB per NV index, and nitro-tpm-attest needs 8 KiB (aws/NitroTPM-Tools#7)."
  type        = string
  default     = "c7g.medium"
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
