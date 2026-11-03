variable "name" {
  description = "Name prefix, e.g. cloud-desktops-dev."
  type        = string
}

variable "vpc_id" {
  type = string
}

variable "ssm_prefix" {
  description = "SSM prefix, e.g. /cloud-desktops/dev. Builds publish image measurements under <prefix>/gateway/images/."
  type        = string
}

variable "permissions_boundary" {
  description = "Permissions boundary for the role (a customer deployment's: deploy/customer-aws). Null: none."
  type        = string
  default     = null
}
